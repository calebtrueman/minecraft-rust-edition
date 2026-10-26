//! Unsafe reference compare-and-set for instance and static fields.

use crate::JavaError::NullPointerException;
use crate::{Error, Result, Thread};
use ristretto_classfile::JavaStr;
use ristretto_classloader::Value;
use ristretto_types::{BoxFuture, Parameters};
use std::sync::Arc;

const STATIC_FIELD_OFFSET_MASK: i64 = 1 << 62;

pub(crate) fn compare_and_set_reference(
    thread: Arc<Thread>,
    parameters: Parameters,
) -> BoxFuture<'static, Result<Option<Value>>> {
    Box::pin(async move {
        let mut arguments = parameters.clone();
        let new_value = arguments.pop()?;
        let expected = arguments.pop()?;
        let offset = arguments.pop_long()?;
        let base = arguments.pop()?;

        if offset & STATIC_FIELD_OFFSET_MASK == 0 {
            return ristretto_intrinsics::jdk::internal::misc::r#unsafe::compare_and_set_reference(
                thread, parameters,
            );
        }

        if base.is_null() {
            return Err(NullPointerException(Some(
                "compareAndSetReference: Reference cannot be null".to_string(),
            ))
            .into());
        }

        let class_name = base.as_object_ref()?.value("name")?.as_string()?;
        // The VM has no mirror-to-Class reverse lookup. Resolve through the active caller loader,
        // then verify the exact mirror so a same-named class from another loader cannot be updated.
        let class = thread
            .load_and_link_class(&JavaStr::cow_from_str(&class_name))
            .await?;
        let class_mirror = class.object()?.ok_or_else(|| {
            Error::InternalError(format!("Class mirror is missing for {class_name}"))
        })?;
        if !same_object(&base, &class_mirror) {
            return Err(Error::InternalError(format!(
                "Static reference CAS class mirror does not match {class_name}"
            )));
        }

        let field_offset = usize::try_from(offset & !STATIC_FIELD_OFFSET_MASK)?;
        let field_name = class.field_name(field_offset)?;
        let changed = class.compare_and_set_static_reference(&field_name, &expected, new_value)?;
        Ok(Some(Value::Int(i32::from(changed))))
    })
}

fn same_object(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(Some(left)), Value::Object(Some(right))) => left.ptr_eq(right),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    }
}
