//! A first Rust-owned piece of the Java runtime Minecraft: Rust Edition would need.
//!
//! This crate reads JVM class files and can run a small set of static methods
//! that take and return `int`. It is not a general JVM and does not load Fabric
//! mods. See `docs/java-mod-compatibility.md` for the intended next steps.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, error::Error, fmt};

const MAX_CLASS_BYTES: usize = 16 << 20;
const MAX_CODE_BYTES: usize = u16::MAX as usize;
const MAX_FRAME_SLOTS: usize = 4096;
const MAX_CALL_DEPTH: usize = 64;
const MAX_INSTRUCTION_STEPS: u64 = 10_000_000;
const MAX_LOADED_CLASSES: usize = 1024;
const MAX_LOADED_CLASS_BYTES: usize = 128 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmError(String);

impl VmError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for VmError {}

type Result<T> = std::result::Result<T, VmError>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Constant {
    Empty,
    Utf8(String),
    Class(u16),
    NameAndType { name: u16, descriptor: u16 },
    MethodRef { class: u16, name_and_type: u16 },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub max_stack: u16,
    pub max_locals: u16,
    pub exception_handlers: u16,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub code: Option<Code>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    pub name: String,
    pub superclass: Option<String>,
    pub methods: BTreeMap<(String, String), Method>,
    constants: Vec<Constant>,
}

#[derive(Debug, Default)]
pub struct Runtime {
    classes: BTreeMap<String, ClassFile>,
    loaded_bytes: usize,
}

#[derive(Debug)]
struct InstructionBudget {
    steps: u64,
    limit: u64,
}

struct Invocation<'a> {
    class_name: &'a str,
    method_name: &'a str,
    descriptor: &'a str,
    args: &'a [i32],
    caller: Option<&'a str>,
    depth: usize,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and add one class. Callers choose which class bytes to load.
    pub fn load_class(&mut self, bytes: &[u8]) -> Result<String> {
        if self.classes.len() >= MAX_LOADED_CLASSES {
            return Err(VmError::new(
                "loaded class count exceeds the 1024-class limit",
            ));
        }
        if self.loaded_bytes.saturating_add(bytes.len()) > MAX_LOADED_CLASS_BYTES {
            return Err(VmError::new("loaded class bytes exceed the 128 MiB limit"));
        }
        let class = ClassFile::parse(bytes)?;
        if self.classes.contains_key(&class.name) {
            return Err(VmError::new(format!(
                "class already loaded: {}",
                class.name
            )));
        }
        let name = class.name.clone();
        self.loaded_bytes += bytes.len();
        self.classes.insert(name.clone(), class);
        Ok(name)
    }

    /// Run a loaded static method with only `int` arguments and an `int` result.
    /// The instruction budget is shared by every method call and capped at ten million.
    pub fn run_static_i32(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[i32],
        step_limit: u64,
    ) -> Result<i32> {
        let mut budget = InstructionBudget {
            steps: 0,
            limit: step_limit.min(MAX_INSTRUCTION_STEPS),
        };
        self.invoke_static(
            Invocation {
                class_name,
                method_name,
                descriptor,
                args,
                caller: None,
                depth: 0,
            },
            &mut budget,
        )
    }

    fn invoke_static(&self, call: Invocation<'_>, budget: &mut InstructionBudget) -> Result<i32> {
        if call.depth >= MAX_CALL_DEPTH {
            return Err(VmError::new("method call depth limit reached"));
        }
        let class = self
            .classes
            .get(call.class_name)
            .ok_or_else(|| VmError::new(format!("class not loaded: {}", call.class_name)))?;
        let method = class
            .methods
            .get(&(call.method_name.to_owned(), call.descriptor.to_owned()))
            .ok_or_else(|| {
                VmError::new(format!(
                    "method not found: {}.{}{}",
                    call.class_name, call.method_name, call.descriptor
                ))
            })?;
        if method.access_flags & 0x0008 == 0 {
            return Err(VmError::new(format!(
                "method is not static: {}.{}{}",
                call.class_name, call.method_name, call.descriptor
            )));
        }
        if method.access_flags & 0x0020 != 0 {
            return Err(VmError::new("synchronized methods are not supported"));
        }
        let accessible = match call.caller {
            Some(caller) => method_accessible(method.access_flags, caller, call.class_name),
            None => method.access_flags & 0x0001 != 0,
        };
        if !accessible {
            return Err(VmError::new(format!(
                "method access denied: {}.{}{}",
                call.class_name, call.method_name, call.descriptor
            )));
        }
        if class.methods.keys().any(|(name, _)| name == "<clinit>") {
            return Err(VmError::new(format!(
                "class initialization is not supported: {}",
                call.class_name
            )));
        }
        match class.superclass.as_deref() {
            Some("java/lang/Object") if class.name != "java/lang/Object" => {}
            None if class.name == "java/lang/Object" => {}
            _ => {
                return Err(VmError::new(format!(
                    "only classes directly based on java/lang/Object can run: {}",
                    call.class_name
                )));
            }
        }
        if method_descriptor_arity(call.descriptor)? != call.args.len() {
            return Err(VmError::new("argument count does not match the descriptor"));
        }
        let code = method
            .code
            .as_ref()
            .ok_or_else(|| VmError::new("method has no Code attribute"))?;
        run(self, class, code, call.args, budget, call.depth)
    }
}

impl ClassFile {
    /// Read the class structure and each method's `Code` attribute.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CLASS_BYTES {
            return Err(VmError::new("class file exceeds the 16 MiB limit"));
        }
        let mut r = Reader::new(bytes);
        if r.u4()? != 0xCAFE_BABE {
            return Err(VmError::new("not a JVM class file"));
        }
        let minor_version = r.u2()?;
        let major_version = r.u2()?;
        let constants = read_constants(&mut r)?;

        r.u2()?; // class access flags
        let name = class_name(&constants, r.u2()?)?;
        let super_index = r.u2()?;
        let superclass = if super_index == 0 {
            None
        } else {
            Some(class_name(&constants, super_index)?)
        };

        let interface_count = r.u2()? as usize;
        for _ in 0..interface_count {
            class_name(&constants, r.u2()?)?;
        }
        skip_members(&mut r, &constants)?; // fields

        let mut methods = BTreeMap::new();
        let method_count = r.u2()? as usize;
        for _ in 0..method_count {
            let access_flags = r.u2()?;
            let method_name = utf8(&constants, r.u2()?)?.to_owned();
            let descriptor = utf8(&constants, r.u2()?)?.to_owned();
            let attribute_count = r.u2()? as usize;
            let mut code = None;
            for _ in 0..attribute_count {
                let attribute_name = utf8(&constants, r.u2()?)?;
                let length = r.u4()? as usize;
                let mut body = Reader::new(r.take(length)?);
                if attribute_name == "Code" {
                    if code.is_some() {
                        return Err(VmError::new("method has more than one Code attribute"));
                    }
                    code = Some(read_code(&mut body, &constants)?);
                } else {
                    body.skip(body.remaining())?;
                }
                if body.remaining() != 0 {
                    return Err(VmError::new("method attribute has trailing bytes"));
                }
            }
            let method = Method {
                access_flags,
                name: method_name.clone(),
                descriptor: descriptor.clone(),
                code,
            };
            if methods.insert((method_name, descriptor), method).is_some() {
                return Err(VmError::new("duplicate method name and descriptor"));
            }
        }
        skip_attributes(&mut r, &constants)?;
        if r.remaining() != 0 {
            return Err(VmError::new("class file has trailing bytes"));
        }
        Ok(Self {
            minor_version,
            major_version,
            name,
            superclass,
            methods,
            constants,
        })
    }

    /// Run one static method with only `int` arguments and an `int` result.
    /// `step_limit` bounds loops and is capped at ten million instructions.
    /// Unsupported instructions fail with an error.
    pub fn run_static_i32(
        &self,
        name: &str,
        descriptor: &str,
        args: &[i32],
        step_limit: u64,
    ) -> Result<i32> {
        let mut runtime = Runtime::new();
        runtime.classes.insert(self.name.clone(), self.clone());
        runtime.run_static_i32(&self.name, name, descriptor, args, step_limit)
    }

    fn method_ref(&self, index: u16) -> Result<(String, String, String)> {
        let (class_index, name_and_type_index) = match self.constants.get(index as usize) {
            Some(Constant::MethodRef {
                class,
                name_and_type,
            }) => (*class, *name_and_type),
            _ => {
                return Err(VmError::new(
                    "constant-pool index is not a method reference",
                ));
            }
        };
        let (name_index, descriptor_index) = match self.constants.get(name_and_type_index as usize)
        {
            Some(Constant::NameAndType { name, descriptor }) => (*name, *descriptor),
            _ => return Err(VmError::new("method reference has no name and type")),
        };
        Ok((
            class_name(&self.constants, class_index)?,
            utf8(&self.constants, name_index)?.to_owned(),
            utf8(&self.constants, descriptor_index)?.to_owned(),
        ))
    }
}

fn read_constants(r: &mut Reader<'_>) -> Result<Vec<Constant>> {
    let count = r.u2()? as usize;
    if count == 0 {
        return Err(VmError::new("constant pool cannot be empty"));
    }
    let mut entries = Vec::with_capacity(count);
    entries.push(Constant::Empty); // constant pool indexes start at one
    while entries.len() < count {
        match r.u1()? {
            1 => {
                let length = r.u2()? as usize;
                entries.push(Constant::Utf8(decode_modified_utf8(r.take(length)?)?));
            }
            7 => entries.push(Constant::Class(r.u2()?)),
            3 | 4 | 9 | 17 | 18 => {
                r.skip(4)?;
                entries.push(Constant::Other);
            }
            10 | 11 => entries.push(Constant::MethodRef {
                class: r.u2()?,
                name_and_type: r.u2()?,
            }),
            12 => entries.push(Constant::NameAndType {
                name: r.u2()?,
                descriptor: r.u2()?,
            }),
            8 | 16 | 19 | 20 => {
                r.skip(2)?;
                entries.push(Constant::Other);
            }
            15 => {
                r.skip(3)?;
                entries.push(Constant::Other);
            }
            5 | 6 => {
                r.skip(8)?;
                entries.push(Constant::Other);
                if entries.len() < count {
                    entries.push(Constant::Empty); // long and double use two indexes
                } else {
                    return Err(VmError::new("wide constant is missing its second slot"));
                }
            }
            tag => return Err(VmError::new(format!("unknown constant-pool tag {tag}"))),
        }
    }
    Ok(entries)
}

fn utf8(constants: &[Constant], index: u16) -> Result<&str> {
    match constants.get(index as usize) {
        Some(Constant::Utf8(s)) => Ok(s),
        _ => Err(VmError::new("constant-pool index is not a UTF-8 string")),
    }
}

fn class_name(constants: &[Constant], index: u16) -> Result<String> {
    match constants.get(index as usize) {
        Some(Constant::Class(name_index)) => Ok(utf8(constants, *name_index)?.to_owned()),
        _ => Err(VmError::new("constant-pool index is not a class")),
    }
}

fn skip_members(r: &mut Reader<'_>, constants: &[Constant]) -> Result<()> {
    let count = r.u2()? as usize;
    for _ in 0..count {
        r.skip(2)?; // access flags
        utf8(constants, r.u2()?)?;
        utf8(constants, r.u2()?)?; // field name and descriptor
        skip_attributes(r, constants)?;
    }
    Ok(())
}

fn skip_attributes(r: &mut Reader<'_>, constants: &[Constant]) -> Result<()> {
    let count = r.u2()? as usize;
    for _ in 0..count {
        utf8(constants, r.u2()?)?;
        let length = r.u4()? as usize;
        r.skip(length)?;
    }
    Ok(())
}

fn read_code(r: &mut Reader<'_>, constants: &[Constant]) -> Result<Code> {
    let max_stack = r.u2()?;
    let max_locals = r.u2()?;
    let length = r.u4()? as usize;
    if length == 0 || length > MAX_CODE_BYTES {
        return Err(VmError::new(
            "method code is empty or exceeds the JVM 65535-byte limit",
        ));
    }
    let bytes = r.take(length)?.to_vec();
    let exceptions = r.u2()? as usize;
    r.skip(
        exceptions
            .checked_mul(8)
            .ok_or_else(|| VmError::new("exception table is too large"))?,
    )?;
    skip_attributes(r, constants)?;
    Ok(Code {
        max_stack,
        max_locals,
        exception_handlers: exceptions as u16,
        bytes,
    })
}

fn decode_modified_utf8(bytes: &[u8]) -> Result<String> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let first = bytes[at];
        match first {
            0x01..=0x7f => {
                units.push(first as u16);
                at += 1;
            }
            0xc0..=0xdf => {
                let second = *bytes
                    .get(at + 1)
                    .ok_or_else(|| VmError::new("truncated modified UTF-8"))?;
                if second & 0xc0 != 0x80 {
                    return Err(VmError::new("invalid modified UTF-8 continuation"));
                }
                let unit = (((first & 0x1f) as u16) << 6) | (second & 0x3f) as u16;
                if unit == 0 {
                    if first != 0xc0 || second != 0x80 {
                        return Err(VmError::new("invalid modified UTF-8 null"));
                    }
                } else if unit < 0x80 {
                    return Err(VmError::new("overlong modified UTF-8 sequence"));
                }
                units.push(unit);
                at += 2;
            }
            0xe0..=0xef => {
                let second = *bytes
                    .get(at + 1)
                    .ok_or_else(|| VmError::new("truncated modified UTF-8"))?;
                let third = *bytes
                    .get(at + 2)
                    .ok_or_else(|| VmError::new("truncated modified UTF-8"))?;
                if second & 0xc0 != 0x80 || third & 0xc0 != 0x80 {
                    return Err(VmError::new("invalid modified UTF-8 continuation"));
                }
                let unit = (((first & 0x0f) as u16) << 12)
                    | (((second & 0x3f) as u16) << 6)
                    | (third & 0x3f) as u16;
                if unit < 0x800 {
                    return Err(VmError::new("overlong modified UTF-8 sequence"));
                }
                units.push(unit);
                at += 3;
            }
            _ => return Err(VmError::new("invalid modified UTF-8 leading byte")),
        }
    }
    String::from_utf16(&units).map_err(|_| VmError::new("invalid UTF-16 in modified UTF-8"))
}

fn method_descriptor_arity(descriptor: &str) -> Result<usize> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return Err(VmError::new(
            "only descriptors with int arguments are supported",
        ));
    }
    let close = bytes
        .iter()
        .position(|b| *b == b')')
        .ok_or_else(|| VmError::new("malformed method descriptor"))?;
    if bytes[1..close].iter().any(|b| *b != b'I') || bytes.get(close + 1..) != Some(b"I") {
        return Err(VmError::new(
            "only int arguments and an int result are supported",
        ));
    }
    Ok(close - 1)
}

fn method_accessible(flags: u16, caller: &str, owner: &str) -> bool {
    if flags & 0x0001 != 0 {
        return true;
    }
    if flags & 0x0002 != 0 {
        return caller == owner;
    }
    fn package(name: &str) -> &str {
        name.rsplit_once('/').map_or("", |(package, _)| package)
    }
    package(caller) == package(owner)
}

fn run(
    runtime: &Runtime,
    class: &ClassFile,
    code: &Code,
    args: &[i32],
    budget: &mut InstructionBudget,
    depth: usize,
) -> Result<i32> {
    if code.exception_handlers != 0 {
        return Err(VmError::new("Java exception handlers are not supported"));
    }
    let stack_limit = code.max_stack as usize;
    let local_count = code.max_locals as usize;
    if stack_limit > MAX_FRAME_SLOTS || local_count > MAX_FRAME_SLOTS {
        return Err(VmError::new("method frame exceeds the 4096-slot limit"));
    }
    if args.len() > local_count {
        return Err(VmError::new(
            "method has too few local slots for its arguments",
        ));
    }
    let boundaries = validate_instructions(&code.bytes)?;
    let mut locals = vec![0_i32; local_count];
    let mut initialized = vec![false; local_count];
    locals[..args.len()].copy_from_slice(args);
    initialized[..args.len()].fill(true);
    let mut stack = Vec::with_capacity(stack_limit.min(256));
    let mut pc = 0_usize;

    loop {
        if budget.steps >= budget.limit {
            return Err(VmError::new("instruction limit reached"));
        }
        budget.steps += 1;
        let start = pc;
        let op = *code
            .bytes
            .get(pc)
            .ok_or_else(|| VmError::new("program counter is outside the method"))?;
        let len = instruction_len(op)?;
        let next = pc + len;
        let mut jump = None;
        match op {
            0x00 => {}
            0x02..=0x08 => push(&mut stack, op as i32 - 0x03, stack_limit)?,
            0x10 => push(&mut stack, code.bytes[pc + 1] as i8 as i32, stack_limit)?,
            0x11 => push(
                &mut stack,
                i16::from_be_bytes([code.bytes[pc + 1], code.bytes[pc + 2]]) as i32,
                stack_limit,
            )?,
            0x15 => load(
                &mut stack,
                &locals,
                &initialized,
                code.bytes[pc + 1] as usize,
                stack_limit,
            )?,
            0x1a..=0x1d => load(
                &mut stack,
                &locals,
                &initialized,
                (op - 0x1a) as usize,
                stack_limit,
            )?,
            0x36 => store(
                &mut stack,
                &mut locals,
                &mut initialized,
                code.bytes[pc + 1] as usize,
            )?,
            0x3b..=0x3e => store(
                &mut stack,
                &mut locals,
                &mut initialized,
                (op - 0x3b) as usize,
            )?,
            0x60 => binary(&mut stack, |a, b| Ok(a.wrapping_add(b)))?,
            0x64 => binary(&mut stack, |a, b| Ok(a.wrapping_sub(b)))?,
            0x68 => binary(&mut stack, |a, b| Ok(a.wrapping_mul(b)))?,
            0x6c => binary(&mut stack, java_div)?,
            0x70 => binary(&mut stack, java_rem)?,
            0x74 => {
                let value = pop(&mut stack)?;
                push(&mut stack, value.wrapping_neg(), stack_limit)?;
            }
            0x84 => {
                let index = code.bytes[pc + 1] as usize;
                let amount = code.bytes[pc + 2] as i8 as i32;
                let value = locals
                    .get_mut(index)
                    .ok_or_else(|| VmError::new("local index is out of range"))?;
                if !initialized[index] {
                    return Err(VmError::new("local variable is not initialized"));
                }
                *value = value.wrapping_add(amount);
            }
            0x99..=0x9e => {
                let value = pop(&mut stack)?;
                if test_zero(op, value) {
                    jump = Some(branch_target(start, &code.bytes)?);
                }
            }
            0x9f..=0xa4 => {
                let right = pop(&mut stack)?;
                let left = pop(&mut stack)?;
                if test_pair(op, left, right) {
                    jump = Some(branch_target(start, &code.bytes)?);
                }
            }
            0xa7 => jump = Some(branch_target(start, &code.bytes)?),
            0xb8 => {
                let reference = u16::from_be_bytes([code.bytes[pc + 1], code.bytes[pc + 2]]);
                let (target_class, target_name, target_descriptor) = class.method_ref(reference)?;
                let arity = method_descriptor_arity(&target_descriptor)?;
                if stack.len() < arity {
                    return Err(VmError::new("operand stack underflow at invokestatic"));
                }
                let first_arg = stack.len() - arity;
                let call_args: Vec<i32> = stack.drain(first_arg..).collect();
                let value = runtime.invoke_static(
                    Invocation {
                        class_name: &target_class,
                        method_name: &target_name,
                        descriptor: &target_descriptor,
                        args: &call_args,
                        caller: Some(&class.name),
                        depth: depth + 1,
                    },
                    budget,
                )?;
                push(&mut stack, value, stack_limit)?;
            }
            0xac => {
                if stack.is_empty() {
                    return Err(VmError::new("operand stack underflow"));
                }
                if stack.len() != 1 {
                    return Err(VmError::new("invalid operand stack at ireturn"));
                }
                return pop(&mut stack);
            }
            _ => return Err(VmError::new(format!("unsupported opcode 0x{op:02x}"))),
        }
        pc = jump.unwrap_or(next);
        if !boundaries.get(pc).copied().unwrap_or(false) {
            return Err(VmError::new("branch target is not an instruction boundary"));
        }
    }
}

fn instruction_len(op: u8) -> Result<usize> {
    match op {
        0x00
        | 0x02..=0x08
        | 0x1a..=0x1d
        | 0x3b..=0x3e
        | 0x60
        | 0x64
        | 0x68
        | 0x6c
        | 0x70
        | 0x74
        | 0x99..=0xa7
        | 0xac => Ok(if (0x99..=0xa7).contains(&op) { 3 } else { 1 }),
        0x10 | 0x15 | 0x36 => Ok(2),
        0x11 | 0x84 | 0xb8 => Ok(3),
        _ => Err(VmError::new(format!("unsupported opcode 0x{op:02x}"))),
    }
}

fn validate_instructions(bytes: &[u8]) -> Result<Vec<bool>> {
    let mut boundaries = vec![false; bytes.len() + 1];
    let mut targets = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        boundaries[at] = true;
        let op = bytes[at];
        let len = instruction_len(op)?;
        if at + len > bytes.len() {
            return Err(VmError::new("truncated bytecode instruction"));
        }
        if (0x99..=0xa7).contains(&op) {
            let offset = i16::from_be_bytes([bytes[at + 1], bytes[at + 2]]) as isize;
            let target = at as isize + offset;
            if target < 0 || target >= bytes.len() as isize {
                return Err(VmError::new("branch target is outside the method"));
            }
            targets.push(target as usize);
        }
        at += len;
    }
    boundaries[bytes.len()] = true;
    if targets.iter().any(|target| !boundaries[*target]) {
        return Err(VmError::new("branch target is not an instruction boundary"));
    }
    Ok(boundaries)
}

fn branch_target(pc: usize, bytes: &[u8]) -> Result<usize> {
    let offset = i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as isize;
    usize::try_from(pc as isize + offset).map_err(|_| VmError::new("negative branch target"))
}

fn test_zero(op: u8, value: i32) -> bool {
    match op {
        0x99 => value == 0,
        0x9a => value != 0,
        0x9b => value < 0,
        0x9c => value >= 0,
        0x9d => value > 0,
        0x9e => value <= 0,
        _ => false,
    }
}

fn test_pair(op: u8, left: i32, right: i32) -> bool {
    match op {
        0x9f => left == right,
        0xa0 => left != right,
        0xa1 => left < right,
        0xa2 => left >= right,
        0xa3 => left > right,
        0xa4 => left <= right,
        _ => false,
    }
}

fn push(stack: &mut Vec<i32>, value: i32, limit: usize) -> Result<()> {
    if stack.len() >= limit {
        return Err(VmError::new("operand stack exceeds max_stack"));
    }
    stack.push(value);
    Ok(())
}

fn pop(stack: &mut Vec<i32>) -> Result<i32> {
    stack
        .pop()
        .ok_or_else(|| VmError::new("operand stack underflow"))
}

fn load(
    stack: &mut Vec<i32>,
    locals: &[i32],
    initialized: &[bool],
    index: usize,
    limit: usize,
) -> Result<()> {
    if !initialized
        .get(index)
        .copied()
        .ok_or_else(|| VmError::new("local index is out of range"))?
    {
        return Err(VmError::new("local variable is not initialized"));
    }
    push(
        stack,
        *locals
            .get(index)
            .ok_or_else(|| VmError::new("local index is out of range"))?,
        limit,
    )
}

fn store(
    stack: &mut Vec<i32>,
    locals: &mut [i32],
    initialized: &mut [bool],
    index: usize,
) -> Result<()> {
    let value = pop(stack)?;
    *locals
        .get_mut(index)
        .ok_or_else(|| VmError::new("local index is out of range"))? = value;
    initialized[index] = true;
    Ok(())
}

fn binary(stack: &mut Vec<i32>, op: impl FnOnce(i32, i32) -> Result<i32>) -> Result<()> {
    let right = pop(stack)?;
    let left = pop(stack)?;
    stack.push(op(left, right)?);
    Ok(())
}

fn java_div(left: i32, right: i32) -> Result<i32> {
    if right == 0 {
        return Err(VmError::new("integer division by zero"));
    }
    Ok(if left == i32::MIN && right == -1 {
        i32::MIN
    } else {
        left / right
    })
}

fn java_rem(left: i32, right: i32) -> Result<i32> {
    if right == 0 {
        return Err(VmError::new("integer remainder by zero"));
    }
    Ok(if left == i32::MIN && right == -1 {
        0
    } else {
        left % right
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| VmError::new("class file size overflow"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| VmError::new("truncated class file"))?;
        self.at = end;
        Ok(bytes)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(drop)
    }
    fn u1(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u2(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u4(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_runs_static_integer_methods_with_branches() {
        let class = ClassFile::parse(&fixture(&[
            ("scaled_sum", "(II)I", &[0x1a, 0x1b, 0x60, 0x06, 0x68, 0xac]),
            (
                "positive",
                "(I)I",
                &[0x1a, 0x9b, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac],
            ),
        ]))
        .unwrap();
        assert_eq!(class.name, "fixture/Math");
        assert_eq!(class.superclass.as_deref(), Some("java/lang/Object"));
        assert_eq!(
            class.run_static_i32("scaled_sum", "(II)I", &[2, 6], 20),
            Ok(24)
        );
        assert_eq!(class.run_static_i32("positive", "(I)I", &[4], 20), Ok(1));
        assert_eq!(class.run_static_i32("positive", "(I)I", &[-4], 20), Ok(0));
    }

    #[test]
    fn rejects_bad_class_files_and_unsupported_instructions() {
        assert!(ClassFile::parse(&[0, 1, 2]).is_err());
        let class = ClassFile::parse(&fixture(&[("call", "()I", &[0xbb, 0xac])])).unwrap();
        let error = class.run_static_i32("call", "()I", &[], 10).unwrap_err();
        assert!(error.to_string().contains("unsupported opcode 0xbb"));
    }

    #[test]
    fn execution_is_bounded_and_checks_stack() {
        let class = ClassFile::parse(&fixture(&[("loop", "()I", &[0xa7, 0x00, 0x00])])).unwrap();
        assert_eq!(
            class
                .run_static_i32("loop", "()I", &[], 3)
                .unwrap_err()
                .to_string(),
            "instruction limit reached"
        );
        let class = ClassFile::parse(&fixture(&[("bad", "()I", &[0xac])])).unwrap();
        assert_eq!(
            class
                .run_static_i32("bad", "()I", &[], 10)
                .unwrap_err()
                .to_string(),
            "operand stack underflow"
        );
    }

    #[test]
    fn rejects_bad_branches_locals_and_exception_handlers() {
        let branch =
            ClassFile::parse(&fixture(&[("bad", "()I", &[0xa7, 0x00, 0x01, 0xac])])).unwrap();
        assert!(
            branch
                .run_static_i32("bad", "()I", &[], 10)
                .unwrap_err()
                .to_string()
                .contains("instruction boundary")
        );

        let local = ClassFile::parse(&fixture(&[("bad", "()I", &[0x1a, 0xac])])).unwrap();
        assert_eq!(
            local
                .run_static_i32("bad", "()I", &[], 10)
                .unwrap_err()
                .to_string(),
            "local variable is not initialized"
        );

        let handled =
            ClassFile::parse(&fixture_with_handlers(&[("handled", "()I", &[0x04, 0xac])])).unwrap();
        assert_eq!(
            handled
                .run_static_i32("handled", "()I", &[], 10)
                .unwrap_err()
                .to_string(),
            "Java exception handlers are not supported"
        );

        let initialized = ClassFile::parse(&fixture(&[
            ("<clinit>", "()V", &[0xb1]),
            ("value", "()I", &[0x04, 0xac]),
        ]))
        .unwrap();
        assert_eq!(
            initialized
                .run_static_i32("value", "()I", &[], 10)
                .unwrap_err()
                .to_string(),
            "class initialization is not supported: fixture/Math"
        );
    }

    #[test]
    fn matches_java_integer_overflow_edges() {
        assert_eq!(java_div(i32::MIN, -1), Ok(i32::MIN));
        assert_eq!(java_rem(i32::MIN, -1), Ok(0));
    }

    #[test]
    fn runtime_calls_loaded_static_methods_and_reports_missing_targets() {
        let caller = caller_fixture("fixture/Helper", "apply");
        let helper = fixture_named(
            "fixture/Helper",
            &[("apply", "(I)I", &[0x1a, 0x05, 0x68, 0xac])],
        );
        let mut runtime = Runtime::new();
        assert_eq!(runtime.load_class(&caller), Ok("fixture/Caller".to_owned()));
        assert_eq!(runtime.load_class(&helper), Ok("fixture/Helper".to_owned()));
        assert_eq!(
            runtime.run_static_i32("fixture/Caller", "call", "(I)I", &[7], 20),
            Ok(14)
        );
        assert_eq!(
            runtime
                .run_static_i32("fixture/Caller", "call", "(I)I", &[7], 3)
                .unwrap_err()
                .to_string(),
            "instruction limit reached"
        );

        let error = Runtime::new()
            .run_static_i32("fixture/Absent", "run", "()I", &[], 20)
            .unwrap_err();
        assert_eq!(error.to_string(), "class not loaded: fixture/Absent");

        let mut missing_class = Runtime::new();
        missing_class.load_class(&caller).unwrap();
        assert_eq!(
            missing_class
                .run_static_i32("fixture/Caller", "call", "(I)I", &[7], 20)
                .unwrap_err()
                .to_string(),
            "class not loaded: fixture/Helper"
        );

        let wrong_helper = fixture_named("fixture/Helper", &[("different", "(I)I", &[0x1a, 0xac])]);
        let mut missing_method = Runtime::new();
        missing_method.load_class(&caller).unwrap();
        missing_method.load_class(&wrong_helper).unwrap();
        assert_eq!(
            missing_method
                .run_static_i32("fixture/Caller", "call", "(I)I", &[7], 20)
                .unwrap_err()
                .to_string(),
            "method not found: fixture/Helper.apply(I)I"
        );
    }

    #[test]
    fn recursive_static_calls_stop_at_the_frame_limit() {
        let caller = caller_fixture("fixture/Caller", "call");
        let mut runtime = Runtime::new();
        runtime.load_class(&caller).unwrap();
        assert_eq!(
            runtime
                .run_static_i32("fixture/Caller", "call", "(I)I", &[7], 10_000)
                .unwrap_err()
                .to_string(),
            "method call depth limit reached"
        );
    }

    #[test]
    fn class_loading_stops_at_the_runtime_limit() {
        let mut runtime = Runtime::new();
        for i in 0..MAX_LOADED_CLASSES {
            let name = format!("fixture/Empty{i}");
            runtime.load_class(&fixture_named(&name, &[])).unwrap();
        }
        let extra = fixture_named("fixture/Extra", &[]);
        assert_eq!(
            runtime.load_class(&extra).unwrap_err().to_string(),
            "loaded class count exceeds the 1024-class limit"
        );
    }

    #[test]
    fn decodes_modified_utf8_null_and_supplementary_characters() {
        assert_eq!(decode_modified_utf8(&[0xc0, 0x80]).unwrap(), "\0");
        assert_eq!(
            decode_modified_utf8(&[0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80]).unwrap(),
            "\u{1f600}"
        );
        assert!(decode_modified_utf8(&[0]).is_err());
    }

    #[test]
    fn integer_division_by_zero_is_a_vm_error() {
        let class =
            ClassFile::parse(&fixture(&[("divide", "(II)I", &[0x1a, 0x1b, 0x6c, 0xac])])).unwrap();
        assert_eq!(
            class
                .run_static_i32("divide", "(II)I", &[7, 0], 10)
                .unwrap_err()
                .to_string(),
            "integer division by zero"
        );
    }

    fn fixture(methods: &[(&str, &str, &[u8])]) -> Vec<u8> {
        fixture_named("fixture/Math", methods)
    }

    fn fixture_with_handlers(methods: &[(&str, &str, &[u8])]) -> Vec<u8> {
        fixture_bytes("fixture/Math", methods, true)
    }

    fn fixture_named(class_name: &str, methods: &[(&str, &str, &[u8])]) -> Vec<u8> {
        fixture_bytes(class_name, methods, false)
    }

    fn fixture_bytes(
        class_name: &str,
        methods: &[(&str, &str, &[u8])],
        with_handler: bool,
    ) -> Vec<u8> {
        fn utf(pool: &mut Vec<Vec<u8>>, value: &str) -> u16 {
            let mut entry = vec![1];
            entry.extend((value.len() as u16).to_be_bytes());
            entry.extend(value.as_bytes());
            pool.push(entry);
            pool.len() as u16
        }
        fn class(pool: &mut Vec<Vec<u8>>, value: &str) -> u16 {
            let name = utf(pool, value);
            let mut entry = vec![7];
            entry.extend(name.to_be_bytes());
            pool.push(entry);
            pool.len() as u16
        }
        let mut pool = Vec::new();
        let this = class(&mut pool, class_name);
        let parent = class(&mut pool, "java/lang/Object");
        let code_name = utf(&mut pool, "Code");
        let specs: Vec<_> = methods
            .iter()
            .map(|(name, descriptor, code)| {
                (utf(&mut pool, name), utf(&mut pool, descriptor), *code)
            })
            .collect();

        let mut out = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69];
        out.extend(((pool.len() + 1) as u16).to_be_bytes());
        for entry in pool {
            out.extend(entry);
        }
        out.extend([0, 0x21]);
        out.extend(this.to_be_bytes());
        out.extend(parent.to_be_bytes());
        out.extend([0, 0, 0, 0]); // interfaces and fields
        out.extend((specs.len() as u16).to_be_bytes());
        for (name, descriptor, code) in specs {
            out.extend([0, 9]); // public static
            out.extend(name.to_be_bytes());
            out.extend(descriptor.to_be_bytes());
            out.extend([0, 1]);
            out.extend(code_name.to_be_bytes());
            out.extend(((12 + code.len() + if with_handler { 8 } else { 0 }) as u32).to_be_bytes());
            out.extend([0, 2, 0, 2]); // max_stack, max_locals
            out.extend((code.len() as u32).to_be_bytes());
            out.extend(code);
            out.extend((u16::from(with_handler)).to_be_bytes()); // exception table length
            if with_handler {
                out.extend([0, 0, 0, code.len() as u8]); // protected range
                out.extend([0, 0, 0, 0]); // handler and catch-all type
            }
            out.extend([0, 0]); // nested attributes
        }
        out.extend([0, 0]); // class attributes
        out
    }

    fn caller_fixture(target_class: &str, target_method: &str) -> Vec<u8> {
        fn utf(pool: &mut Vec<Vec<u8>>, value: &str) -> u16 {
            let mut entry = vec![1];
            entry.extend((value.len() as u16).to_be_bytes());
            entry.extend(value.as_bytes());
            pool.push(entry);
            pool.len() as u16
        }
        fn class(pool: &mut Vec<Vec<u8>>, value: &str) -> u16 {
            let name = utf(pool, value);
            let mut entry = vec![7];
            entry.extend(name.to_be_bytes());
            pool.push(entry);
            pool.len() as u16
        }

        let mut pool = Vec::new();
        let this = class(&mut pool, "fixture/Caller");
        let parent = class(&mut pool, "java/lang/Object");
        let code_name = utf(&mut pool, "Code");
        let call_name = utf(&mut pool, "call");
        let call_descriptor = utf(&mut pool, "(I)I");
        let target_class_index = class(&mut pool, target_class);
        let target_name = utf(&mut pool, target_method);
        let target_descriptor = utf(&mut pool, "(I)I");
        let mut name_and_type = vec![12];
        name_and_type.extend(target_name.to_be_bytes());
        name_and_type.extend(target_descriptor.to_be_bytes());
        pool.push(name_and_type);
        let name_and_type_index = pool.len() as u16;
        let mut method_ref = vec![10];
        method_ref.extend(target_class_index.to_be_bytes());
        method_ref.extend(name_and_type_index.to_be_bytes());
        pool.push(method_ref);
        let method_ref_index = pool.len() as u16;
        let code = [
            0x1a,
            0xb8,
            (method_ref_index >> 8) as u8,
            method_ref_index as u8,
            0xac,
        ];

        let mut out = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69];
        out.extend(((pool.len() + 1) as u16).to_be_bytes());
        for entry in pool {
            out.extend(entry);
        }
        out.extend([0, 0x21]);
        out.extend(this.to_be_bytes());
        out.extend(parent.to_be_bytes());
        out.extend([0, 0, 0, 0]); // interfaces and fields
        out.extend([0, 1]); // one method
        out.extend([0, 9]); // public static
        out.extend(call_name.to_be_bytes());
        out.extend(call_descriptor.to_be_bytes());
        out.extend([0, 1]);
        out.extend(code_name.to_be_bytes());
        out.extend(((12 + code.len()) as u32).to_be_bytes());
        out.extend([0, 1, 0, 1]); // max_stack, max_locals
        out.extend((code.len() as u32).to_be_bytes());
        out.extend(code);
        out.extend([0, 0, 0, 0]); // exception table and nested attributes
        out.extend([0, 0]); // class attributes
        out
    }
}
