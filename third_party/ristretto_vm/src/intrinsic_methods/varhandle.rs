//! Local VarHandle handling for byte-array views used during Java class loading.

use crate::Error::InternalError;
use crate::JavaError::{ArrayIndexOutOfBoundsException, ClassCastException, NullPointerException};
use crate::{Result, Thread};
use ristretto_classloader::{Reference, Value};
use ristretto_types::{BoxFuture, Parameters};
use std::sync::Arc;

/// Handle layouts used by `MethodHandles.byteArrayViewVarHandle`.
#[derive(Clone, Copy)]
enum ByteViewKind {
    Short,
    Char,
    Int,
    Float,
    Long,
    Double,
}

impl ByteViewKind {
    fn from_class_name(class_name: &str) -> Option<Self> {
        let view = class_name
            .strip_prefix("java/lang/invoke/VarHandleByteArrayAs")?
            .strip_suffix("$ArrayHandle")?;
        match view {
            "Shorts" => Some(Self::Short),
            "Chars" => Some(Self::Char),
            "Ints" => Some(Self::Int),
            "Floats" => Some(Self::Float),
            "Longs" => Some(Self::Long),
            "Doubles" => Some(Self::Double),
            _ => None,
        }
    }

    const fn width(self) -> usize {
        match self {
            Self::Short | Self::Char => 2,
            Self::Int | Self::Float => 4,
            Self::Long | Self::Double => 8,
        }
    }

    fn value(self, bits: u64) -> Result<Value> {
        match self {
            Self::Short => Ok(Value::Int(i32::from(i16::from_ne_bytes(
                u16::try_from(bits)?.to_ne_bytes(),
            )))),
            Self::Char => Ok(Value::Int(i32::from(u16::try_from(bits)?))),
            Self::Int => Ok(Value::Int(i32::from_ne_bytes(
                u32::try_from(bits)?.to_ne_bytes(),
            ))),
            Self::Float => Ok(Value::Float(f32::from_bits(u32::try_from(bits)?))),
            Self::Long => Ok(Value::Long(i64::from_ne_bytes(bits.to_ne_bytes()))),
            Self::Double => Ok(Value::Double(f64::from_bits(bits))),
        }
    }
}

/// Run plain `VarHandle.get` for Java's byte-array view handles.
///
/// The JDK implementation checks that the complete value fits in the byte array. It permits
/// unaligned plain reads, so this path deliberately does not apply the alignment checks used by
/// stronger access modes.
fn try_byte_array_view_get(parameters: &[Value]) -> Result<Option<Value>> {
    let Some(handle) = parameters.first() else {
        return Ok(None);
    };
    let Ok(handle_ref) = handle.as_object_ref() else {
        return Ok(None);
    };
    let class_name = handle_ref.class().name().to_string();
    let Some(kind) = ByteViewKind::from_class_name(&class_name) else {
        return Ok(None);
    };

    let big_endian = handle_ref.value("be")?.as_bool()?;
    let target = parameters
        .get(1)
        .ok_or_else(|| InternalError("VarHandle byte-array view: missing array".into()))?;
    let index = parameters
        .get(2)
        .ok_or_else(|| InternalError("VarHandle byte-array view: missing byte index".into()))?
        .as_i32()?;

    let target_ref = match target {
        Value::Object(None) => return Err(NullPointerException(None).into()),
        Value::Object(Some(_)) => target.as_reference()?,
        _ => {
            return Err(InternalError(
                "VarHandle byte-array view: expected an object array target".into(),
            ));
        }
    };
    let bytes = match &*target_ref {
        Reference::ByteArray(bytes) => bytes,
        reference => {
            return Err(ClassCastException {
                source_class_name: reference.class_name()?,
                target_class_name: "[B".into(),
            }
            .into());
        }
    };

    let width = kind.width();
    let offset = usize::try_from(index).ok();
    let Some(offset) = offset.filter(|offset| {
        offset
            .checked_add(width)
            .is_some_and(|end| end <= bytes.len())
    }) else {
        return Err(ArrayIndexOutOfBoundsException {
            index,
            // The JDK checks the index against `array.length - width + 1`.
            length: bytes.len().saturating_sub(width - 1),
        }
        .into());
    };

    let slice = &bytes[offset..offset + width];
    let mut bits = 0_u64;
    if big_endian {
        for byte in slice {
            bits = (bits << 8) | u64::from(byte.to_ne_bytes()[0]);
        }
    } else {
        for byte in slice.iter().rev() {
            bits = (bits << 8) | u64::from(byte.to_ne_bytes()[0]);
        }
    }
    Ok(Some(kind.value(bits)?))
}

/// Handle byte-array view reads directly, and use the shared implementation for other handles.
pub(crate) fn get(
    thread: Arc<Thread>,
    parameters: Parameters,
) -> BoxFuture<'static, Result<Option<Value>>> {
    Box::pin(async move {
        if let Some(value) = try_byte_array_view_get(parameters.as_slice())? {
            return Ok(Some(value));
        }
        ristretto_intrinsics::java::lang::invoke::varhandle::get(thread, parameters).await
    })
}
