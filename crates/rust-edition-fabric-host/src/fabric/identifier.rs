//! A small Rust implementation of vanilla 26.3 `Identifier` string behavior.
//!
//! It deliberately leaves codec and Brigadier fields empty, and does not run the game's
//! `Identifier.<clinit>` or method bodies.

// Ristretto's native callback API returns its large error type directly.
#![allow(clippy::result_large_err)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ristretto_vm::{Error as VmError, JavaError, Object};
use rust_edition_java::{
    Class, HostClassInitializer, HostMethod, NativeResult, Parameters, Thread, Value,
};

const CLASS: &str = "net/minecraft/resources/Identifier";
const EXCEPTION_CLASS: &str = "net/minecraft/IdentifierException";
const DEFAULT_NAMESPACE: &str = "minecraft";
const HOST_FUTURE: &str = "net.minecraft.resources.Identifier.<host callback>";

type HostFuture = Pin<Box<dyn Future<Output = NativeResult<Option<Value>>> + Send + 'static>>;
type InitializerFuture = Pin<Box<dyn Future<Output = NativeResult<()>> + Send + 'static>>;
type HostCallback = fn(Arc<Thread>, Parameters) -> HostFuture;

pub(super) fn methods() -> Vec<HostMethod> {
    let bindings: Vec<(&str, HostCallback)> = vec![
        (
            "fromNamespaceAndPath(Ljava/lang/String;Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            from_namespace_and_path,
        ),
        (
            "parse(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            parse,
        ),
        (
            "withDefaultNamespace(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            with_default_namespace,
        ),
        (
            "tryParse(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            try_parse,
        ),
        (
            "tryBuild(Ljava/lang/String;Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            try_build,
        ),
        (
            "bySeparator(Ljava/lang/String;C)Lnet/minecraft/resources/Identifier;",
            by_separator,
        ),
        (
            "tryBySeparator(Ljava/lang/String;C)Lnet/minecraft/resources/Identifier;",
            try_by_separator,
        ),
        ("isAllowedInIdentifier(C)Z", is_allowed_in_identifier),
        ("isValidPath(Ljava/lang/String;)Z", is_valid_path),
        ("isValidNamespace(Ljava/lang/String;)Z", is_valid_namespace),
        ("validPathChar(C)Z", valid_path_char),
        ("getPath()Ljava/lang/String;", get_path),
        ("getNamespace()Ljava/lang/String;", get_namespace),
        (
            "withPath(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            with_path,
        ),
        (
            "withPrefix(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            with_prefix,
        ),
        (
            "withSuffix(Ljava/lang/String;)Lnet/minecraft/resources/Identifier;",
            with_suffix,
        ),
        ("toString()Ljava/lang/String;", to_string),
        ("equals(Ljava/lang/Object;)Z", equals),
        ("hashCode()I", hash_code),
        (
            "compareTo(Lnet/minecraft/resources/Identifier;)I",
            compare_to,
        ),
        ("toDebugFileName()Ljava/lang/String;", to_debug_file_name),
        ("toLanguageKey()Ljava/lang/String;", to_language_key),
        (
            "toShortLanguageKey()Ljava/lang/String;",
            to_short_language_key,
        ),
        ("toShortString()Ljava/lang/String;", to_short_string),
        (
            "toLanguageKey(Ljava/lang/String;)Ljava/lang/String;",
            to_language_key_with_prefix,
        ),
        (
            "toLanguageKey(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            to_language_key_with_prefix_and_suffix,
        ),
    ];
    bindings
        .into_iter()
        .map(|(method, callback)| HostMethod::asynchronous(format!("{CLASS}.{method}"), callback))
        .collect()
}

pub(super) fn initializers() -> [HostClassInitializer; 1] {
    [HostClassInitializer::new(
        CLASS,
        Arc::new(
            |_thread: Arc<Thread>, _class: Arc<Class>| -> InitializerFuture {
                // ConstantValue fields are already set by the VM. CODEC, STREAM_CODEC, and
                // ERROR_INVALID stay null because their implementations would require the game's
                // codec and Brigadier behavior.
                Box::pin(async { Ok(()) })
            },
        ),
    )]
}

fn from_namespace_and_path(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let path_value = parameters.pop()?;
        let namespace_value = parameters.pop()?;
        let namespace = take_required_string_value(namespace_value, NPE_NAMESPACE_EQUALS)?;
        let path = if valid_namespace(&namespace) {
            take_required_string_value(path_value, NPE_PATH_LENGTH)?
        } else {
            take_concat_string_value(path_value)?
        };
        create_identifier(thread, namespace, path, false).await
    })
}

fn parse(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let identifier = take_required_string(&mut parameters, NPE_IDENTIFIER_INDEX)?;
        create_by_separator(thread, identifier, b':' as u16, false).await
    })
}

fn with_default_namespace(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let path = take_required_string(&mut parameters, NPE_PATH_LENGTH)?;
        create_identifier(thread, DEFAULT_NAMESPACE.to_owned(), path, false).await
    })
}

fn try_parse(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let identifier = take_required_string(&mut parameters, NPE_IDENTIFIER_INDEX)?;
        create_by_separator(thread, identifier, b':' as u16, true).await
    })
}

fn try_build(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let path_value = parameters.pop()?;
        let namespace_value = parameters.pop()?;
        let namespace = take_required_string_value(namespace_value, NPE_NAMESPACE_EQUALS)?;
        if !valid_namespace(&namespace) {
            return Ok(Some(Value::Object(None)));
        }
        let path = take_required_string_value(path_value, NPE_PATH_LENGTH)?;
        create_identifier(thread, namespace, path, true).await
    })
}

fn by_separator(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let separator = take_java_separator(&mut parameters)?;
        let identifier = take_required_string(&mut parameters, NPE_IDENTIFIER_INDEX)?;
        create_by_separator(thread, identifier, separator, false).await
    })
}

fn try_by_separator(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let separator = take_java_separator(&mut parameters)?;
        let identifier = take_required_string(&mut parameters, NPE_IDENTIFIER_INDEX)?;
        create_by_separator(thread, identifier, separator, true).await
    })
}

fn is_allowed_in_identifier(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let character = parameters.pop_int()?;
        Ok(Some(Value::from(is_identifier_char(character))))
    })
}

fn is_valid_path(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let path = take_required_string(&mut parameters, NPE_PATH_LENGTH)?;
        Ok(Some(Value::from(valid_path(&path))))
    })
}

fn is_valid_namespace(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let namespace = take_required_string(&mut parameters, NPE_NAMESPACE_EQUALS)?;
        Ok(Some(Value::from(valid_namespace(&namespace))))
    })
}

fn valid_path_char(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let character = parameters.pop_int()?;
        Ok(Some(Value::from(is_path_char(character))))
    })
}

fn get_path(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        Ok(Some(identifier_field(&receiver, "path")?))
    })
}

fn get_namespace(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        Ok(Some(identifier_field(&receiver, "namespace")?))
    })
}

fn with_path(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let path = take_required_string(&mut parameters, NPE_PATH_LENGTH)?;
        let receiver = parameters.pop()?;
        let (namespace, _) = identifier_parts(&receiver)?;
        create_identifier(thread, namespace, path, false).await
    })
}

fn with_prefix(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let prefix = take_concat_string(&mut parameters)?;
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        create_identifier(thread, namespace, format!("{prefix}{path}"), false).await
    })
}

fn with_suffix(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let suffix = take_concat_string(&mut parameters)?;
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        create_identifier(thread, namespace, format!("{path}{suffix}"), false).await
    })
}

fn to_string(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        Ok(Some(
            java_string(&thread, &format!("{namespace}:{path}")).await?,
        ))
    })
}

fn equals(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let other = parameters.pop()?;
        let receiver = parameters.pop()?;
        if same_object(&receiver, &other) {
            return Ok(Some(Value::from(true)));
        }
        let Some(other_class) = object_class_name(&other)? else {
            return Ok(Some(Value::from(false)));
        };
        if other_class != CLASS {
            return Ok(Some(Value::from(false)));
        }
        let equal = identifier_parts(&receiver)? == identifier_parts(&other)?;
        Ok(Some(Value::from(equal)))
    })
}

fn hash_code(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        let hash = java_string_hash(&namespace)
            .wrapping_mul(31)
            .wrapping_add(java_string_hash(&path));
        Ok(Some(Value::Int(hash)))
    })
}

fn compare_to(_thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let other = parameters.pop()?;
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        let (other_namespace, other_path) = identifier_parts(&other)?;
        let path_order = java_string_compare(&path, &other_path);
        let order = if path_order == 0 {
            java_string_compare(&namespace, &other_namespace)
        } else {
            path_order
        };
        Ok(Some(Value::Int(order)))
    })
}

fn to_debug_file_name(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        let text = format!("{namespace}:{path}").replace(['/', ':'], "_");
        Ok(Some(java_string(&thread, &text).await?))
    })
}

fn to_language_key(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        Ok(Some(
            java_string(&thread, &format!("{namespace}.{path}")).await?,
        ))
    })
}

fn to_short_language_key(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        let text = if namespace == DEFAULT_NAMESPACE {
            path
        } else {
            format!("{namespace}.{path}")
        };
        Ok(Some(java_string(&thread, &text).await?))
    })
}

fn to_short_string(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        let text = if namespace == DEFAULT_NAMESPACE {
            path
        } else {
            format!("{namespace}:{path}")
        };
        Ok(Some(java_string(&thread, &text).await?))
    })
}

fn to_language_key_with_prefix(thread: Arc<Thread>, mut parameters: Parameters) -> HostFuture {
    Box::pin(async move {
        let prefix = take_concat_string(&mut parameters)?;
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        Ok(Some(
            java_string(&thread, &format!("{prefix}.{namespace}.{path}")).await?,
        ))
    })
}

fn to_language_key_with_prefix_and_suffix(
    thread: Arc<Thread>,
    mut parameters: Parameters,
) -> HostFuture {
    Box::pin(async move {
        let suffix = take_concat_string(&mut parameters)?;
        let prefix = take_concat_string(&mut parameters)?;
        let receiver = parameters.pop()?;
        let (namespace, path) = identifier_parts(&receiver)?;
        Ok(Some(
            java_string(&thread, &format!("{prefix}.{namespace}.{path}.{suffix}")).await?,
        ))
    })
}

async fn create_by_separator(
    thread: Arc<Thread>,
    identifier: String,
    separator: u16,
    try_only: bool,
) -> NativeResult<Option<Value>> {
    let (namespace, path) = match identifier.encode_utf16().position(|unit| unit == separator) {
        Some(index) if index > 0 => split_at_utf16(&identifier, index, index + 1)?,
        Some(index) => {
            let (_, path) = split_at_utf16(&identifier, index, index + 1)?;
            (DEFAULT_NAMESPACE.to_owned(), path)
        }
        None => (DEFAULT_NAMESPACE.to_owned(), identifier),
    };
    create_identifier(thread, namespace, path, try_only).await
}

fn split_at_utf16(value: &str, before: usize, after: usize) -> NativeResult<(String, String)> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    let left = String::from_utf16(units.get(..before).ok_or_else(internal_bad_text)?)
        .map_err(|_| internal_bad_text())?;
    let right = String::from_utf16(units.get(after..).ok_or_else(internal_bad_text)?)
        .map_err(|_| internal_bad_text())?;
    Ok((left, right))
}

async fn create_identifier(
    thread: Arc<Thread>,
    namespace: String,
    path: String,
    try_only: bool,
) -> NativeResult<Option<Value>> {
    if let Err(message) = validate(&namespace, &path) {
        if try_only {
            return Ok(Some(Value::Object(None)));
        }
        let exception = identifier_exception(&thread, &message).await?;
        return Err(VmError::Throwable(exception));
    }

    let vm = thread.vm()?;
    let class = vm.load_and_link_class(CLASS).await?;
    let object = Value::from_object(vm.garbage_collector(), Object::new(class)?);
    let namespace = java_string(&thread, &namespace).await?;
    let path = java_string(&thread, &path).await?;
    {
        let mut object = object.as_object_mut()?;
        object.set_value("namespace", namespace)?;
        object.set_value("path", path)?;
    }
    Ok(Some(object))
}

fn validate(namespace: &str, path: &str) -> Result<(), String> {
    if !valid_namespace(namespace) {
        return Err(format!(
            "Non [a-z0-9_.-] character in namespace of identifier: {namespace}:{path}"
        ));
    }
    if !valid_path(path) {
        return Err(format!(
            "Non [a-z0-9/._-] character in path of location: {namespace}:{path}"
        ));
    }
    Ok(())
}

fn valid_namespace(namespace: &str) -> bool {
    namespace != ".." && namespace.bytes().all(is_namespace_byte)
}

fn valid_path(path: &str) -> bool {
    path.bytes().all(is_path_byte)
}

fn is_namespace_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_path_byte(byte: u8) -> bool {
    is_namespace_byte(byte) || byte == b'/'
}

fn is_identifier_char(value: i32) -> bool {
    (b'0' as i32..=b'9' as i32).contains(&value)
        || (b'a' as i32..=b'z' as i32).contains(&value)
        || matches!(value, 95 | 58 | 47 | 46 | 45)
}

fn is_path_char(value: i32) -> bool {
    (b'0' as i32..=b'9' as i32).contains(&value)
        || (b'a' as i32..=b'z' as i32).contains(&value)
        || matches!(value, 95 | 45 | 47 | 46)
}

async fn identifier_exception(thread: &Thread, message: &str) -> NativeResult<Value> {
    let vm = thread.vm()?;
    let class = vm.load_and_link_class(EXCEPTION_CLASS).await?;
    let throwable_class = vm.load_and_link_class("java/lang/Throwable").await?;
    let exception = Value::from_object(vm.garbage_collector(), Object::new(class)?);
    let message = java_string(thread, message).await?;
    {
        let mut object = exception.as_object_mut()?;
        object.set_value_in_class(&throwable_class, "detailMessage", message)?;
        object.set_value_in_class(&throwable_class, "cause", exception.clone())?;
    }
    let policy = vm
        .configuration()
        .method_execution_policy()
        .ok_or_else(|| {
            VmError::InternalError("Identifier host hooks need a method policy".into())
        })?;
    policy.allow_inherited_method_on_host_object(
        &exception,
        "java/lang/Throwable.getMessage()Ljava/lang/String;",
    )?;
    policy.allow_inherited_method_on_host_object(
        &exception,
        "java/lang/Object.getClass()Ljava/lang/Class;",
    )?;
    policy.allow_inherited_field_read_on_host_object(
        &exception,
        "java/lang/Throwable.detailMessage:Ljava/lang/String;",
    )?;
    Ok(exception)
}

async fn java_string(thread: &Thread, value: &str) -> NativeResult<Value> {
    thread
        .object("java/lang/String", "Ljava/lang/String;", &[value])
        .await
}

fn identifier_field(receiver: &Value, field: &str) -> NativeResult<Value> {
    let object = receiver.as_object_ref()?;
    if object.class().name() != CLASS {
        return Err(VmError::InternalError(format!(
            "{HOST_FUTURE} expected an Identifier receiver, found {}",
            object.class().name()
        )));
    }
    Ok(object.value(field)?)
}

fn identifier_parts(receiver: &Value) -> NativeResult<(String, String)> {
    if matches!(receiver, Value::Object(None)) {
        return Err(JavaError::NullPointerException(None).into());
    }
    let namespace = identifier_field(receiver, "namespace")?.as_string()?;
    let path = identifier_field(receiver, "path")?.as_string()?;
    Ok((namespace, path))
}

fn object_class_name(value: &Value) -> NativeResult<Option<String>> {
    match value {
        Value::Object(None) => Ok(None),
        Value::Object(Some(_)) => Ok(Some(value.as_object_ref()?.class().name().to_owned())),
        _ => Err(VmError::InternalError(format!(
            "{HOST_FUTURE} expected an object argument"
        ))),
    }
}

fn same_object(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(Some(left)), Value::Object(Some(right))) => left.ptr_eq(right),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    }
}

fn take_required_string(parameters: &mut Parameters, null_message: &str) -> NativeResult<String> {
    take_required_string_value(parameters.pop()?, null_message)
}

fn take_required_string_value(value: Value, null_message: &str) -> NativeResult<String> {
    match value {
        Value::Object(None) => {
            Err(JavaError::NullPointerException(Some(null_message.to_owned())).into())
        }
        value => value.as_string().map_err(Into::into),
    }
}

fn take_concat_string(parameters: &mut Parameters) -> NativeResult<String> {
    take_concat_string_value(parameters.pop()?)
}

fn take_concat_string_value(value: Value) -> NativeResult<String> {
    match value {
        Value::Object(None) => Ok("null".to_owned()),
        value => value.as_string().map_err(Into::into),
    }
}

fn take_java_separator(parameters: &mut Parameters) -> NativeResult<u16> {
    let separator = parameters.pop_int()?;
    if (0..=u16::MAX as i32).contains(&separator) {
        Ok(separator as u16)
    } else {
        Err(VmError::InternalError(format!(
            "{HOST_FUTURE} received a char outside the UTF-16 range"
        )))
    }
}

fn java_string_hash(value: &str) -> i32 {
    value.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}

fn java_string_compare(left: &str, right: &str) -> i32 {
    for (left, right) in left.encode_utf16().zip(right.encode_utf16()) {
        if left != right {
            return i32::from(left) - i32::from(right);
        }
    }
    let left_length = left.encode_utf16().count();
    let right_length = right.encode_utf16().count();
    left_length as i32 - right_length as i32
}

fn internal_bad_text() -> VmError {
    VmError::InternalError(format!("{HOST_FUTURE} received a split UTF-16 surrogate"))
}

const NPE_IDENTIFIER_INDEX: &str =
    "Cannot invoke \"String.indexOf(int)\" because \"identifier\" is null";
const NPE_NAMESPACE_EQUALS: &str =
    "Cannot invoke \"String.equals(Object)\" because \"namespace\" is null";
const NPE_PATH_LENGTH: &str = "Cannot invoke \"String.length()\" because \"path\" is null";
