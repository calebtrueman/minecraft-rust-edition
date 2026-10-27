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
    AsyncIntrinsicMethod, Class, HostClassInitializer as HostClassInitializerCallback,
    HostClassInitializerFuture, MethodExecutionPolicy, Object, Parameters, Reference, RustValue,
    SyncIntrinsicMethod, Thread, Value,
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

/// One Rust implementation that replaces the body of one exact Java method.
#[derive(Clone, Debug)]
pub struct HostMethod {
    signature: String,
    callback: IntrinsicMethod,
}

impl HostMethod {
    /// Replace one exact Java method with a synchronous Rust function.
    ///
    /// The signature uses an internal class name, a dot, the method name, and its JVM descriptor,
    /// such as `example/Host.answer()I`. The Java method may be ordinary or `native`.
    #[must_use]
    pub fn sync(signature: impl Into<String>, callback: SyncIntrinsicMethod) -> Self {
        Self {
            signature: signature.into(),
            callback: IntrinsicMethod::Sync(callback),
        }
    }

    /// Replace one exact Java method with an async Rust function.
    #[must_use]
    pub fn asynchronous(signature: impl Into<String>, callback: AsyncIntrinsicMethod) -> Self {
        Self {
            signature: signature.into(),
            callback: IntrinsicMethod::Async(callback),
        }
    }
}

/// One Rust replacement for the Java `<clinit>` of an exact class.
#[derive(Clone)]
pub struct HostClassInitializer {
    class_name: String,
    callback: HostClassInitializerCallback,
}

impl HostClassInitializer {
    /// Replace one class's Java `<clinit>` with this Rust callback.
    ///
    /// Use the Java internal class name, such as `example/Host`. The callback receives the live
    /// VM thread and class so it can create Java values and fill the class's static fields.
    #[must_use]
    pub fn new(class_name: impl Into<String>, callback: HostClassInitializerCallback) -> Self {
        Self {
            class_name: class_name.into(),
            callback,
        }
    }
}

impl fmt::Debug for HostClassInitializer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostClassInitializer")
            .field("class_name", &self.class_name)
            .finish_non_exhaustive()
    }
}

/// A Java 25 VM that can run classes from caller-supplied paths.
pub struct JavaVm {
    vm: std::sync::Arc<VM>,
    method_policy: Option<MethodExecutionPolicy>,
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
        Self::new_inner(
            java_home,
            class_paths,
            native_methods,
            std::iter::empty(),
            std::iter::empty(),
            None,
            None,
        )
        .await
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
            std::iter::empty(),
            std::iter::empty(),
            Some(host_context),
            None,
        )
        .await
    }

    /// Start an interpreted Java 25 VM with caller-owned native state and a method execution
    /// policy.
    ///
    /// The policy blocks methods and direct field instructions on selected Java classes. A host
    /// can allow exact methods and direct field reads on specific object identities. This is a
    /// narrow bytecode guard, not a sandbox: it does not restrict reflection, `Unsafe`, or other
    /// ways Java code may interact with its environment.
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
            std::iter::empty(),
            std::iter::empty(),
            Some(host_context),
            Some(method_policy),
        )
        .await
    }

    /// Start an interpreted Java 25 VM with native methods, exact host method replacements,
    /// class initializer replacements, caller-owned state, and a method policy.
    ///
    /// Host methods run in place of the exact Java method body. Host class initializers run after
    /// the superclass and constant fields are ready and use the VM's normal once-only initialization
    /// and failure rules. Native method registrations keep their existing native-only behavior.
    pub async fn new_with_host_hooks<T: Any + Send + Sync>(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
        host_methods: impl IntoIterator<Item = HostMethod>,
        host_class_initializers: impl IntoIterator<Item = HostClassInitializer>,
        host_context: Arc<T>,
        method_policy: MethodExecutionPolicy,
    ) -> Result<Self> {
        let host_context: Arc<dyn Any + Send + Sync> = host_context;
        Self::new_inner(
            java_home,
            class_paths,
            native_methods,
            host_methods,
            host_class_initializers,
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

    /// Load and link a class, then create a zero-filled object without running `<clinit>` or
    /// `<init>`.
    ///
    /// This is for host-owned mirrors whose fields the host fills in. Later Java actions still use
    /// normal class initialization rules.
    ///
    /// # Errors
    /// Returns an error if the class cannot be loaded, linked, or laid out.
    pub async fn allocate_object_without_initialization(&self, class_name: &str) -> Result<Value> {
        let class = self.vm.load_and_link_class(class_name).await?;
        let object = Object::new(class)
            .map_err(ristretto_vm::Error::from)
            .map_err(Error::from)?;
        Ok(Value::from_object(self.vm.garbage_collector(), object))
    }

    /// Set a host-owned Java object's field by name, checking its declared type and VM identity.
    ///
    /// This trusted host operation can set private or final fields while preparing a mirror. Java
    /// bytecode writes remain subject to the configured method policy.
    ///
    /// # Errors
    /// Returns an error if `object` is null or belongs to another VM, the field is missing, or the
    /// value has the wrong type or its class identity cannot be checked in this VM.
    pub async fn set_host_object_field(
        &self,
        object: &Value,
        field_name: &str,
        value: Value,
    ) -> Result<()> {
        self.vm
            .set_host_object_field(object, field_name, value)
            .await
            .map_err(Error::from)
    }

    /// Allow one exact method signature on one object identity under this VM's policy.
    ///
    /// Signatures use an internal class name, a dot, the method name, and its JVM descriptor, such
    /// as `example/Host.read()I`.
    ///
    /// # Errors
    /// Returns an error if this VM has no method policy or the object or signature is invalid.
    pub fn allow_method_on_host_object(&self, object: &Value, signature: &str) -> Result<()> {
        let policy = self.method_policy.as_ref().ok_or_else(|| {
            Error::Configuration("this Java VM has no host method policy".to_owned())
        })?;
        policy
            .allow_method_on_host_object(object, signature)
            .map_err(Error::from)
    }

    /// Allow one exact method on this object, including a method declared by a superclass or
    /// interface in its loaded class hierarchy.
    ///
    /// This is useful for a Rust-created exception object that needs a small set of inherited
    /// Java getters. It does not allow the same method on another object.
    pub fn allow_inherited_method_on_host_object(
        &self,
        object: &Value,
        signature: &str,
    ) -> Result<()> {
        let policy = self.method_policy.as_ref().ok_or_else(|| {
            Error::Configuration("this Java VM has no host method policy".to_owned())
        })?;
        policy
            .allow_inherited_method_on_host_object(object, signature)
            .map_err(Error::from)
    }

    /// Allow one exact direct field read from one object identity under this VM's policy.
    ///
    /// Signatures use `owner.field:descriptor`, such as `example/Host.value:I`. This does not
    /// allow bytecode writes or reflection access.
    ///
    /// # Errors
    /// Returns an error if this VM has no method policy or the object or signature is invalid.
    pub fn allow_field_read_on_host_object(&self, object: &Value, signature: &str) -> Result<()> {
        let policy = self.method_policy.as_ref().ok_or_else(|| {
            Error::Configuration("this Java VM has no host method policy".to_owned())
        })?;
        policy
            .allow_field_read_on_host_object(object, signature)
            .map_err(Error::from)
    }

    /// Allow one exact direct field read on this object, including a field declared by a
    /// superclass in its loaded class hierarchy.
    pub fn allow_inherited_field_read_on_host_object(
        &self,
        object: &Value,
        signature: &str,
    ) -> Result<()> {
        let policy = self.method_policy.as_ref().ok_or_else(|| {
            Error::Configuration("this Java VM has no host method policy".to_owned())
        })?;
        policy
            .allow_inherited_field_read_on_host_object(object, signature)
            .map_err(Error::from)
    }

    async fn new_inner(
        java_home: impl AsRef<Path>,
        class_paths: &[PathBuf],
        native_methods: impl IntoIterator<Item = NativeMethod>,
        host_methods: impl IntoIterator<Item = HostMethod>,
        host_class_initializers: impl IntoIterator<Item = HostClassInitializer>,
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
        let stored_method_policy = method_policy.clone();
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

        let mut host_signatures = HashSet::new();
        for method in host_methods {
            if method.signature.contains(".<clinit>(") {
                return Err(Error::Configuration(format!(
                    "class initializers must use HostClassInitializer: {}",
                    method.signature
                )));
            }
            if !host_signatures.insert(method.signature.clone()) {
                return Err(Error::Configuration(format!(
                    "duplicate Rust host method binding: {}",
                    method.signature
                )));
            }
            if signatures.contains(&method.signature) {
                return Err(Error::Configuration(format!(
                    "native and host method bindings share a signature: {}",
                    method.signature
                )));
            }
            builder = builder.register_host_method(&method.signature, method.callback);
        }

        let mut initializer_classes = HashSet::new();
        for initializer in host_class_initializers {
            if !initializer_classes.insert(initializer.class_name.clone()) {
                return Err(Error::Configuration(format!(
                    "duplicate Rust host class initializer: {}",
                    initializer.class_name
                )));
            }
            builder = builder
                .register_host_class_initializer(&initializer.class_name, initializer.callback);
        }

        let vm = VM::new(builder.build()?).await?;
        if vm.java_major_version() != 25 {
            return Err(Error::Configuration(format!(
                "Java library home resolved to Java {}, expected Java 25",
                vm.java_version()
            )));
        }
        Ok(Self {
            vm,
            method_policy: stored_method_policy,
        })
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
