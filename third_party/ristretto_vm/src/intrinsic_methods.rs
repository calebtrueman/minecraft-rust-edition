//! # Intrinsic Methods Module
//!
//! This module contains the registry and lookup functionality for intrinsic method
//! implementations. The actual implementations are in the `ristretto_intrinsics` crate.

/// Mapping of intrinsic method names to their implementations
mod intrinsics;
/// Reflection shims for JMOD-only Java homes.
mod jmod_reflection;
/// Registry for storing and retrieving intrinsic method implementations
mod registry;
/// Corrects the Java 21+ Unix `access0` return value.
#[cfg(target_family = "unix")]
mod unix_access;

/// Re-export the `MethodRegistry` for use by other modules in the VM
pub use registry::{AsyncIntrinsicMethod, IntrinsicMethod, MethodRegistry, SyncIntrinsicMethod};
