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
    let allowed_source = source_dir.join("policyfixture/allowed/Allowed.java");
    let caller_source = source_dir.join("policyfixture/Caller.java");
    let classes = scratch.join("classes");
    fs::create_dir_all(target_source.parent().expect("target source parent"))?;
    fs::create_dir_all(allowed_source.parent().expect("allowed source parent"))?;
    fs::create_dir_all(caller_source.parent().expect("caller source parent"))?;
    fs::create_dir_all(&classes)?;
    fs::write(&target_source, TARGET_SOURCE)?;
    fs::write(&initializer_source, INITIALIZER_SOURCE)?;
    fs::write(&allowed_source, ALLOWED_SOURCE)?;
    fs::write(&caller_source, CALLER_SOURCE)?;

    let output = Command::new(javac)
        .args(["--release", "25", "-g:none", "-d"])
        .arg(&classes)
        .arg(&target_source)
        .arg(&initializer_source)
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
