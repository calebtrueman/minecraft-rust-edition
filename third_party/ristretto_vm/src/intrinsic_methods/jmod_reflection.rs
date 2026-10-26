//! Reflection behavior needed when a Java home has JMODs but no `lib/modules` image.

use crate::{Result, Thread};
use ristretto_classfile::attributes::Attribute;
use ristretto_classloader::Value;
use ristretto_types::{BoxFuture, Frame, JavaObject, Parameters};
use std::sync::Arc;

/// Keeps the upstream caller lookup, but avoids its system-image probe for JMOD homes.
pub(crate) fn get_caller_class(
    thread: Arc<Thread>,
    parameters: Parameters,
) -> BoxFuture<'static, Result<Option<Value>>> {
    Box::pin(async move {
        if !thread.vm()?.module_system().uses_jmod_fallback() {
            return ristretto_intrinsics::jdk::internal::reflect::reflection::get_caller_class(
                thread, parameters,
            )
            .await;
        }

        let frames = thread.frames()?;
        for frame in frames.iter().rev().skip(1) {
            let class = frame.class();
            let class_name = class.name();
            if class_name.starts_with("jdk/internal/reflect/")
                || class_name.starts_with("sun/reflect/")
                || is_caller_sensitive_frame(frame.as_ref())?
            {
                continue;
            }
            return Ok(Some(class.to_object(&thread).await?));
        }

        Ok(Some(Value::Object(None)))
    })
}

fn is_caller_sensitive_frame<F: Frame>(frame: &F) -> Result<bool> {
    let class_file = frame.class().class_file();
    let constant_pool = &class_file.constant_pool;
    for attribute in &frame.method().definition().attributes {
        let (Attribute::RuntimeVisibleAnnotations { annotations, .. }
        | Attribute::RuntimeInvisibleAnnotations { annotations, .. }) = attribute
        else {
            continue;
        };
        for annotation in annotations {
            let annotation_type = constant_pool.try_get_utf8(annotation.type_index)?;
            if annotation_type == "Ljdk/internal/reflect/CallerSensitive;"
                || annotation_type == "Lsun/reflect/CallerSensitive;"
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
