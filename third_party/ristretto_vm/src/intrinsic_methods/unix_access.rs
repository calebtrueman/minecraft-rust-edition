//! Small compatibility fixes for Java's Unix file system calls.

use crate::{Result, Thread};
use ristretto_classloader::Value;
use ristretto_types::Parameters;
use std::sync::Arc;

/// Keep the upstream `access0` behavior, but return errno instead of libc's `-1` error marker.
///
/// Java 21 and later expect this native call to return zero on success and the OS error number on
/// failure. The upstream intrinsic returns libc's raw result, so a missing path becomes `-1` and
/// Java reports it as "Unknown error: -1". Read the thread-local OS error as soon as the upstream
/// call returns, before any other host call can replace it.
pub(crate) fn access_0_1(thread: Arc<Thread>, parameters: Parameters) -> Result<Option<Value>> {
    let result =
        ristretto_intrinsics::sun::nio::fs::unixnativedispatcher::access_0_1(thread, parameters)?;

    match result {
        Some(Value::Int(-1)) => {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(5);
            Ok(Some(Value::Int(errno)))
        }
        result => Ok(result),
    }
}
