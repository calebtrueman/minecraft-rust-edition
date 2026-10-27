use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ristretto_vm::{
    ClassPath, ConfigurationBuilder, Error as VmError, JavaError, MethodExecutionPolicy, Object,
    Reference, VM, Value,
};
use rust_edition_java::{Error as AdapterError, JavaVm, NativeMethod};

const TARGET_CLASS: &str = "policyfixture.blocked.Target";
const INITIALIZER_CLASS: &str = "policyfixture.blocked.InitializerTarget";
const MIRROR_INTERNAL: &str = "policyfixture/blocked/Mirror";
const CALLER_CLASS: &str = "policyfixture.Caller";
const ALLOWED_CLASS: &str = "policyfixture.allowed.Allowed";

const TARGET_SOURCE: &str = r#"
package policyfixture.blocked;

public final class Target {
    public static int staticValue() { return 31; }
    public int instanceValue() { return 32; }
}
"#;

const INITIALIZER_SOURCE: &str = r#"
package policyfixture.blocked;

public final class InitializerTarget {
    static { failIfInitializerRuns(); }
    public static int value() { return 61; }

    private static void failIfInitializerRuns() {
        throw new AssertionError("the blocked initializer must not run");
    }
}
"#;

const MIRROR_SOURCE: &str = r#"
package policyfixture.blocked;

public final class Mirror {
    private final Mirror window;
    public int framebufferWidth;
    public int framebufferHeight;
    public int unregistered;
    public static int staticValue = 71;

    static { failIfInitializerRuns(); }
    public Mirror() { throw new AssertionError("host must allocate this mirror without a constructor"); }

    private static void failIfInitializerRuns() {
        throw new AssertionError("host must allocate this mirror without class initialization");
    }

    public Mirror getWindow() { return window; }
    public int getWidth() { return framebufferWidth; }
    public int getHeight() { return framebufferHeight; }
}
"#;

const ALLOWED_SOURCE: &str = r#"
package policyfixture.allowed;

public final class Allowed {
    public static int value() { return 41; }
}
"#;

const CALLER_SOURCE: &str = r#"
package policyfixture;

import policyfixture.allowed.Allowed;
import policyfixture.blocked.InitializerTarget;
import policyfixture.blocked.Mirror;
import policyfixture.blocked.Target;

public final class Caller {
    public static int catchesStaticCall() {
        try { return Target.staticValue(); }
        catch (UnsupportedOperationException error) { return 51; }
    }

    public static int catchesInitializerCall() {
        try { return InitializerTarget.value(); }
        catch (UnsupportedOperationException error) { return 53; }
    }

    public static int catchesInheritedCall(Target target) {
        try { target.toString(); return 0; }
        catch (UnsupportedOperationException error) { return 52; }
    }

    public static Mirror callWindow(Mirror mirror) { return mirror.getWindow(); }
    public static int callWidth(Mirror mirror) { return mirror.getWidth(); }
    public static int callHeight(Mirror mirror) { return mirror.getHeight(); }
    public static int readWidth(Mirror mirror) { return mirror.framebufferWidth; }
    public static int readUnregistered(Mirror mirror) { return mirror.unregistered; }

    public static int catchesWriteWidth(Mirror mirror) {
        try { mirror.framebufferWidth = 7; return 0; }
        catch (UnsupportedOperationException error) { return 97; }
    }

    public static int catchesStaticRead() {
        try { return Mirror.staticValue; }
        catch (UnsupportedOperationException error) { return 98; }
    }

    public static int normalCall() { return Allowed.value(); }
}
"#;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rust-edition-java-method-policy-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn java25_javac() -> Option<PathBuf> {
    let java_home = PathBuf::from(env::var_os("JAVA_HOME")?);
    #[cfg(windows)]
    let binary = "javac.exe";
    #[cfg(not(windows))]
    let binary = "javac";
    let javac = java_home.join("bin").join(binary);
    if !javac.is_file() {
        return None;
    }
    let output = Command::new(&javac).arg("-version").output().ok()?;
    let version = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.status.success().then_some(())?;
    version.contains("javac 25").then_some(javac)
}

fn java25_library_home() -> Option<PathBuf> {
    env::var_os("RUST_EDITION_JAVA25_HOME").map(PathBuf::from)
}

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source_dir = scratch.join("source");
    let target_source = source_dir.join("policyfixture/blocked/Target.java");
    let initializer_source = source_dir.join("policyfixture/blocked/InitializerTarget.java");
    let mirror_source = source_dir.join("policyfixture/blocked/Mirror.java");
    let allowed_source = source_dir.join("policyfixture/allowed/Allowed.java");
    let caller_source = source_dir.join("policyfixture/Caller.java");
    let classes = scratch.join("classes");
    fs::create_dir_all(target_source.parent().expect("target source parent"))?;
    fs::create_dir_all(allowed_source.parent().expect("allowed source parent"))?;
    fs::create_dir_all(caller_source.parent().expect("caller source parent"))?;
    fs::create_dir_all(&classes)?;
    fs::write(&target_source, TARGET_SOURCE)?;
    fs::write(&initializer_source, INITIALIZER_SOURCE)?;
    fs::write(&mirror_source, MIRROR_SOURCE)?;
    fs::write(&allowed_source, ALLOWED_SOURCE)?;
    fs::write(&caller_source, CALLER_SOURCE)?;

    let output = Command::new(javac)
        .args(["--release", "25", "-g:none", "-d"])
        .arg(&classes)
        .arg(&target_source)
        .arg(&initializer_source)
        .arg(&mirror_source)
        .arg(&allowed_source)
        .arg(&caller_source)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "javac failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(classes)
}

fn is_unsupported_operation(error: &AdapterError) -> bool {
    let AdapterError::Ristretto(error) = error else {
        return false;
    };
    match error.as_ref() {
        VmError::JavaError(JavaError::UnsupportedOperationException(_)) => true,
        VmError::Throwable(value) => matches!(
            value.as_reference(),
            Ok(reference)
                if matches!(
                    reference.class_name(),
                    Ok(class_name) if class_name == "java/lang/UnsupportedOperationException"
                )
        ),
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_mirror_policy_is_bound_to_one_object_and_checks_field_types()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME to a Java 25 JDK and RUST_EDITION_JAVA25_HOME to the prepared Java 25 library home"
        );
        return Ok(());
    }
    let javac = java25_javac().ok_or("JAVA_HOME must point to a Java 25 JDK with bin/javac")?;
    let java_home = java25_library_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;
    let scratch = ScratchDir::new()?;
    let class_paths = [compile_fixture(&javac, &scratch.0)?];
    let vm = JavaVm::new_with_host_context_and_policy(
        &java_home,
        &class_paths,
        std::iter::empty::<NativeMethod>(),
        Arc::new(()),
        MethodExecutionPolicy::deny_class_prefixes(["policyfixture.blocked/"]),
    )
    .await?;

    // These allocations do not run the throwing class initializer or constructor.
    let client = vm
        .allocate_object_without_initialization(MIRROR_INTERNAL)
        .await?;
    let window = vm
        .allocate_object_without_initialization(MIRROR_INTERNAL)
        .await?;
    let other_client = vm
        .allocate_object_without_initialization(MIRROR_INTERNAL)
        .await?;
    let other_window = vm
        .allocate_object_without_initialization(MIRROR_INTERNAL)
        .await?;

    vm.set_host_object_field(&window, "framebufferWidth", Value::Int(854))
        .await?;
    vm.set_host_object_field(&window, "framebufferHeight", Value::Int(480))
        .await?;
    vm.set_host_object_field(&client, "window", window.clone())
        .await?;

    let unrelated = vm
        .allocate_object_without_initialization("policyfixture/allowed/Allowed")
        .await?;
    let wrong_type = vm
        .set_host_object_field(&client, "window", unrelated)
        .await
        .expect_err("a same-VM but unrelated object must not fit the Mirror field");
    assert!(wrong_type.to_string().contains("not assignable"));

    let foreign_vm =
        JavaVm::new(&java_home, &class_paths, std::iter::empty::<NativeMethod>()).await?;
    let foreign_mirror = foreign_vm
        .allocate_object_without_initialization(MIRROR_INTERNAL)
        .await?;
    let foreign_destination = vm
        .set_host_object_field(&foreign_mirror, "framebufferWidth", Value::Int(1))
        .await
        .expect_err("host writes must reject objects from a different VM");
    assert!(
        foreign_destination
            .to_string()
            .contains("does not belong to this VM")
    );
    let foreign_value = vm
        .set_host_object_field(&client, "window", foreign_mirror)
        .await
        .expect_err("same-named reference fields must reject values from a different VM");
    assert!(foreign_value.to_string().contains("not assignable"));

    vm.allow_method_on_host_object(
        &client,
        &format!("{MIRROR_INTERNAL}.getWindow()L{MIRROR_INTERNAL};"),
    )?;
    vm.allow_field_read_on_host_object(
        &client,
        &format!("{MIRROR_INTERNAL}.window:L{MIRROR_INTERNAL};"),
    )?;
    vm.allow_method_on_host_object(&window, &format!("{MIRROR_INTERNAL}.getWidth()I"))?;
    vm.allow_method_on_host_object(&window, &format!("{MIRROR_INTERNAL}.getHeight()I"))?;
    vm.allow_field_read_on_host_object(&window, &format!("{MIRROR_INTERNAL}.framebufferWidth:I"))?;
    vm.allow_field_read_on_host_object(&window, &format!("{MIRROR_INTERNAL}.framebufferHeight:I"))?;

    let returned_window = vm
        .invoke(
            CALLER_CLASS,
            &format!("callWindow(L{MIRROR_INTERNAL};)L{MIRROR_INTERNAL};"),
            std::slice::from_ref(&client),
        )
        .await?
        .expect("the helper returns the host window mirror");
    assert!(vm.same_object(&returned_window, &window));

    let width = vm
        .invoke(
            CALLER_CLASS,
            &format!("callWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await?
        .expect("the allowed getter returns a value");
    assert_eq!(854, width.as_i32()?);
    let height = vm
        .invoke(
            CALLER_CLASS,
            &format!("callHeight(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await?
        .expect("the allowed getter returns a value");
    assert_eq!(480, height.as_i32()?);

    // The same Java call site and field reference have already succeeded for the registered
    // objects. They must still reject a different object of the same Java class.
    let unregistered_method = vm
        .invoke(
            CALLER_CLASS,
            &format!("callWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&other_window),
        )
        .await
        .expect_err("an unregistered receiver must not reuse another object's method rule");
    assert!(is_unsupported_operation(&unregistered_method));

    let direct_width = vm
        .invoke(
            CALLER_CLASS,
            &format!("readWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await?
        .expect("the one allowed direct field read returns its value");
    assert_eq!(854, direct_width.as_i32()?);
    let unregistered_field = vm
        .invoke(
            CALLER_CLASS,
            &format!("readWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&other_window),
        )
        .await
        .expect_err("an unregistered receiver must not reuse another object's field rule");
    assert!(is_unsupported_operation(&unregistered_field));

    let other_field = vm
        .invoke(
            CALLER_CLASS,
            &format!("readUnregistered(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await
        .expect_err("a different field on the allowed object must remain blocked");
    assert!(is_unsupported_operation(&other_field));

    let unregistered_method = vm
        .invoke(
            CALLER_CLASS,
            &format!("callWindow(L{MIRROR_INTERNAL};)L{MIRROR_INTERNAL};"),
            std::slice::from_ref(&other_client),
        )
        .await
        .expect_err("the getWindow permission belongs only to the first client object");
    assert!(is_unsupported_operation(&unregistered_method));

    let caught_write = vm
        .invoke(
            CALLER_CLASS,
            &format!("catchesWriteWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await?
        .expect("the Java helper catches a blocked bytecode write");
    assert_eq!(97, caught_write.as_i32()?);
    let width_after_blocked_write = vm
        .invoke(
            CALLER_CLASS,
            &format!("callWidth(L{MIRROR_INTERNAL};)I"),
            std::slice::from_ref(&window),
        )
        .await?
        .expect("the allowed getter still reads the original host value");
    assert_eq!(854, width_after_blocked_write.as_i32()?);

    let caught_static_read = vm
        .invoke(CALLER_CLASS, "catchesStaticRead()I", &[] as &[i32])
        .await?
        .expect("the Java helper catches a blocked static field read");
    assert_eq!(98, caught_static_read.as_i32()?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn method_policy_blocks_calls_without_changing_default_vm_behavior()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME to a Java 25 JDK and RUST_EDITION_JAVA25_HOME to the prepared Java 25 library home"
        );
        return Ok(());
    }
    let javac = java25_javac().ok_or("JAVA_HOME must point to a Java 25 JDK with bin/javac")?;
    let java_home = java25_library_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;
    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let class_paths = [classes];
    let no_native_methods = std::iter::empty::<NativeMethod>();

    let policy_vm = JavaVm::new_with_host_context_and_policy(
        &java_home,
        &class_paths,
        no_native_methods,
        Arc::new(()),
        MethodExecutionPolicy::deny_class_prefixes(["policyfixture.blocked/"]),
    )
    .await?;

    let direct_error = policy_vm
        .invoke(TARGET_CLASS, "staticValue()I", &[] as &[i32])
        .await
        .expect_err("the protected static method must not run");
    match direct_error {
        AdapterError::Ristretto(error) => assert!(matches!(
            error.as_ref(),
            VmError::JavaError(JavaError::UnsupportedOperationException(message))
                if message.contains("policyfixture/blocked/Target.staticValue()I")
        )),
        error => panic!("expected a Java UnsupportedOperationException, got {error}"),
    }

    let initializer_error = policy_vm
        .invoke(INITIALIZER_CLASS, "value()I", &[] as &[i32])
        .await
        .expect_err("the protected class initializer must not run");
    match initializer_error {
        AdapterError::Ristretto(error) => assert!(matches!(
            error.as_ref(),
            VmError::JavaError(JavaError::UnsupportedOperationException(message))
                if message.contains("InitializerTarget.<clinit>()V")
        )),
        error => panic!("expected a Java UnsupportedOperationException, got {error}"),
    }

    let caught = policy_vm
        .invoke(CALLER_CLASS, "catchesStaticCall()I", &[] as &[i32])
        .await?
        .expect("Java catch method returns a value");
    assert_eq!(51, caught.as_i32()?);

    let caught_initializer = policy_vm
        .invoke(CALLER_CLASS, "catchesInitializerCall()I", &[] as &[i32])
        .await?
        .expect("Java catches the blocked initializer request");
    assert_eq!(53, caught_initializer.as_i32()?);

    let allowed = policy_vm
        .invoke(ALLOWED_CLASS, "value()I", &[] as &[i32])
        .await?
        .expect("an unrelated method remains available");
    assert_eq!(41, allowed.as_i32()?);

    // Allocate a real fixture object without calling its blocked constructor. This checks the
    // receiver-class rule for inherited Object methods through Ristretto's public VM API.
    let direct_vm = VM::new(
        ConfigurationBuilder::new()
            .java_home(java_home.clone())
            .class_path(ClassPath::from(&class_paths))
            .interpreted(true)
            .batch_compilation(false)
            .method_execution_policy(MethodExecutionPolicy::deny_class_prefixes([
                "policyfixture.blocked/",
            ]))
            .build()?,
    )
    .await?;
    let target_class = direct_vm.class(TARGET_CLASS).await?;
    let target = Value::new_object(
        direct_vm.garbage_collector(),
        Reference::Object(Object::new(target_class)?),
    );
    let inherited = direct_vm
        .try_invoke(
            CALLER_CLASS,
            "catchesInheritedCall(Lpolicyfixture/blocked/Target;)I",
            std::slice::from_ref(&target),
        )
        .await?;
    assert_eq!(52, inherited.as_i32()?);

    let default_vm =
        JavaVm::new(&java_home, &class_paths, std::iter::empty::<NativeMethod>()).await?;
    let default_static = default_vm
        .invoke(TARGET_CLASS, "staticValue()I", &[] as &[i32])
        .await?
        .expect("default VM keeps its usual behavior");
    assert_eq!(31, default_static.as_i32()?);

    let target = default_vm
        .new_object(TARGET_CLASS, "", &[] as &[i32])
        .await?;
    let default_instance = default_vm
        .invoke(
            TARGET_CLASS,
            "instanceValue()I",
            std::slice::from_ref(&target),
        )
        .await?
        .expect("default VM keeps instance methods available");
    assert_eq!(32, default_instance.as_i32()?);

    let default_allowed = default_vm
        .invoke(CALLER_CLASS, "normalCall()I", &[] as &[i32])
        .await?
        .expect("default VM keeps unrelated methods available");
    assert_eq!(41, default_allowed.as_i32()?);
    Ok(())
}
