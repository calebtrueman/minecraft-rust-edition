//! A small bridge from Java bytecode to Rust callbacks.
//!
//! This crate wraps Ristretto VM 0.34 with Java 25, explicit local class paths, and interpreted
//! execution. It does not load Fabric or connect to the game.

use std::any::Any;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ristretto_vm::{ConfigurationBuilder, IntrinsicMethod, VM};

pub use ristretto_vm::{
    MethodExecutionPolicy, Parameters, RustValue, SyncIntrinsicMethod, Thread, Value,
};

/// The callback result type used by Rust implementations of Java native methods.
pub type NativeResult<T> = ristretto_vm::Result<T>;

/// An adapter setup or Java execution error.
#[derive(Debug)]
pub enum Error {
    /// The supplied paths or native bindings are invalid.
    Configuration(String),
    /// An error returned by Ristretto.
    Ristretto(Box<ristretto_vm::Error>),
}

/// The result type used by [`JavaVm`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => formatter.write_str(message),
            Self::Ristretto(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Configuration(_) => None,
            Self::Ristretto(error) => Some(error.as_ref()),
        }
    }
}

impl From<ristretto_vm::Error> for Error {
    fn from(error: ristretto_vm::Error) -> Self {
        Self::Ristretto(Box::new(error))
    }
}

/// A Rust function for a Java method declared with the `native` keyword.
pub type NativeCallback = SyncIntrinsicMethod;

/// One caller-provided implementation for a Java native method.
#[derive(Clone, Debug)]
pub struct NativeMethod {
    signature: String,
    callback: NativeCallback,
}

impl NativeMethod {
    /// Register a synchronous Rust function for a full Java method signature.
    ///
    /// Use the Java internal class name, a dot, the method name, and its descriptor. For example:
    /// `example/Bridge.hostCall()I`.
    #[must_use]
    pub fn sync(signature: impl Into<String>, callback: NativeCallback) -> Self {
        Self {
            signature: signature.into(),
            callback,
        }
    }
}

/// A Java 25 VM that can run classes from caller-supplied paths.
pub struct JavaVm {
    vm: std::sync::Arc<VM>,
}

impl std::fmt::Debug for JavaVm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JavaVm")
            .field("java_version", &self.java_version())
            .finish()
    }
}

impl JavaVm {
    /// Start an interpreted Java 25 VM with the supplied library home, mod class paths and Rust
    /// native methods.
    ///
    /// `java_home` must point to a Java 25 library home with a `release` file and
    /// `jmods/java.base.jmod`, without `lib/modules`. Use
    /// `tools/prepare_ristretto_java_home.py` to make a small test home from an installed JDK.
    /// The paths in `class_paths` can be directories or JARs. Native signatures use internal
    /// class names, such as `example/Bridge.hostCall()I`. Ristretto calls a registered function
    /// only when bytecode declares that exact method as `native`.
    ///
    /// # Errors
    /// Returns an error if the Java home is missing or not Java 25, a class path is missing, a
    /// binding is repeated, or Ristretto cannot create the VM.
    pub async fn new(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
    ) -> Result<Self> {
        Self::new_inner(java_home, class_paths, native_methods, None, None).await
    }

    /// Start an interpreted Java 25 VM with caller-owned state for its native methods.
    ///
    /// A native method can reach the state with `thread.vm()?.host_context()` and downcast the
    /// returned `Arc`. The state belongs to this adapter; creating another VM with a different
    /// `Arc` gives its callbacks a separate state. Dropping `JavaVm` clears the context. A callback
    /// that already cloned the state keeps it alive until that callback returns. This does not
    /// stop Java threads that Ristretto may still be running.
    ///
    /// See [`JavaVm::new`] for the Java home and class path requirements.
    pub async fn new_with_host_context<T: Any + Send + Sync>(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
        host_context: Arc<T>,
    ) -> Result<Self> {
        let host_context: Arc<dyn Any + Send + Sync> = host_context;
        Self::new_inner(
            java_home,
            class_paths,
            native_methods,
            Some(host_context),
            None,
        )
        .await
    }

    /// Start an interpreted Java 25 VM with caller-owned native state and a method execution
    /// policy.
    ///
    /// The policy can block methods owned by selected Java classes. Ristretto checks every method
    /// call, including calls to inherited methods on instances whose runtime class is blocked.
    /// This is a narrow host guard, not a sandbox: it does not restrict field access, reflection,
    /// or other ways Java code may interact with its environment.
    ///
    /// See [`JavaVm::new`] for the Java home and class path requirements, and
    /// [`JavaVm::new_with_host_context`] for the lifetime of the native state.
    pub async fn new_with_host_context_and_policy<T: Any + Send + Sync>(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
        host_context: Arc<T>,
        method_policy: MethodExecutionPolicy,
    ) -> Result<Self> {
        let host_context: Arc<dyn Any + Send + Sync> = host_context;
        Self::new_inner(
            java_home,
            class_paths,
            native_methods,
            Some(host_context),
            Some(method_policy),
        )
        .await
    }

    /// Returns this VM's context when it has the requested type.
    #[must_use]
    pub fn host_context<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.vm.host_context()?.downcast::<T>().ok()
    }

    /// Returns whether two values hold the same Java object reference.
    ///
    /// This compares reference identity only; it does not check which VM created either value.
    /// Callers should compare values from the same VM. `null` matches `null`, while primitive
    /// values do not have an object identity.
    #[must_use]
    pub fn same_object(&self, left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Object(Some(left)), Value::Object(Some(right))) => left.ptr_eq(right),
            (Value::Object(None), Value::Object(None)) => true,
            _ => false,
        }
    }

    async fn new_inner(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
        host_context: Option<Arc<dyn Any + Send + Sync>>,
        method_policy: Option<MethodExecutionPolicy>,
    ) -> Result<Self> {
        let java_home = java_home.as_ref();
        validate_java_home(java_home)?;
        if class_paths.is_empty() {
            return Err(Error::Configuration(
                "at least one mod class path is required".into(),
            ));
        }
        for class_path in class_paths {
            if !class_path.exists() {
                return Err(Error::Configuration(format!(
                    "mod class path does not exist: {}",
                    class_path.display()
                )));
            }
        }

        let mut builder = ConfigurationBuilder::new()
            .java_home(java_home.to_path_buf())
            .class_path(ristretto_vm::ClassPath::from(class_paths))
            .interpreted(true)
            .batch_compilation(false);
        if let Some(host_context) = host_context {
            builder = builder.host_context(host_context);
        }
        if let Some(method_policy) = method_policy {
            builder = builder.method_execution_policy(method_policy);
        }
        let mut signatures = HashSet::new();
        for method in native_methods {
            if !signatures.insert(method.signature.clone()) {
                return Err(Error::Configuration(format!(
                    "duplicate Rust native method binding: {}",
                    method.signature
                )));
            }
            builder = builder
                .register_native_method(&method.signature, IntrinsicMethod::Sync(method.callback));
        }

        let vm = VM::new(builder.build()?).await?;
        if vm.java_major_version() != 25 {
            return Err(Error::Configuration(format!(
                "Java library home resolved to Java {}, expected Java 25",
                vm.java_version()
            )));
        }
        Ok(Self { vm })
    }

    /// The Java version read by Ristretto from the prepared library home.
    #[must_use]
    pub fn java_version(&self) -> &str {
        self.vm.java_version()
    }

    /// Run a Java method and return its value, if it has one.
    ///
    /// Use a dotted class name, such as `example.Bridge`, and a method signature with its
    /// descriptor, such as `run()I`. Instance methods need their receiver as the first argument.
    ///
    /// # Errors
    /// Returns Java loading, verification or execution errors, including `UnsatisfiedLinkError`
    /// when a `native` method has no matching Rust binding.
    pub async fn invoke(
        &self,
        class_name: &str,
        method_signature: &str,
        arguments: &[impl RustValue],
    ) -> Result<Option<Value>> {
        self.vm
            .invoke(class_name, method_signature, arguments)
            .await
            .map_err(Error::from)
    }

    /// Create a Java object by calling its constructor.
    ///
    /// `parameter_descriptor` is the constructor's JVM parameter list without parentheses or a
    /// return type, such as `Lnet/fabricmc/api/EnvType;`. This is useful for bootstrapping Java
    /// APIs from Rust.
    ///
    /// # Errors
    /// Returns Java loading, verification or execution errors from the constructor.
    pub async fn new_object(
        &self,
        class_name: &str,
        parameter_descriptor: &str,
        arguments: &[impl RustValue],
    ) -> Result<Value> {
        self.vm
            .object(class_name, parameter_descriptor, arguments)
            .await
            .map_err(Error::from)
    }
}

impl Drop for JavaVm {
    fn drop(&mut self) {
        self.vm.clear_host_context();
    }
}

fn validate_java_home(java_home: &Path) -> Result<()> {
    let release_path = java_home.join("release");
    let release = std::fs::read_to_string(&release_path).map_err(|error| {
        Error::Configuration(format!(
            "cannot read prepared Java library release file {}: {error}",
            release_path.display()
        ))
    })?;
    let version = release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .map(|version| version.trim_matches('"'));
    let major = version
        .and_then(|version| version.split(['.', '+', '-']).next())
        .and_then(|major| major.parse::<u16>().ok());
    if major != Some(25) {
        return Err(Error::Configuration(format!(
            "Java library home must be Java 25 (found {})",
            version.unwrap_or("no JAVA_VERSION")
        )));
    }
    if !java_home.join("jmods/java.base.jmod").is_file() || java_home.join("lib/modules").exists() {
        return Err(Error::Configuration(
            "Ristretto needs a Java 25 home with jmods/java.base.jmod and no lib/modules; run tools/prepare_ristretto_java_home.py".into(),
        ));
    }
    Ok(())
}
