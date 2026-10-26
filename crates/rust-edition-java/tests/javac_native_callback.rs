use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_edition_java::{JavaVm, NativeMethod, NativeResult, Parameters, Thread, Value};

const FIXTURE: &str = r#"
public final class NativeFixture {
    public static native int hostCall();

    public static int run() {
        return hostCall();
    }

    public static int ordinaryJava() {
        return 7;
    }
}
"#;

#[allow(
    clippy::result_large_err,
    reason = "Ristretto's native callback signature requires its error type"
)]
fn native_host_call(_thread: Arc<Thread>, _parameters: Parameters) -> NativeResult<Option<Value>> {
    Ok(Some(Value::Int(42)))
}

#[allow(
    clippy::result_large_err,
    reason = "Ristretto's native callback signature requires its error type"
)]
fn callback_that_must_not_replace_bytecode(
    _thread: Arc<Thread>,
    _parameters: Parameters,
) -> NativeResult<Option<Value>> {
    Ok(Some(Value::Int(99)))
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("rust-edition-java-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn javac() -> Option<PathBuf> {
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

fn java25_home() -> Option<PathBuf> {
    env::var_os("RUST_EDITION_JAVA25_HOME").map(PathBuf::from)
}

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source = scratch.join("NativeFixture.java");
    let classes = scratch.join("classes");
    fs::create_dir_all(&classes)?;
    fs::write(&source, FIXTURE)?;
    let output = Command::new(javac)
        .arg("--release")
        .arg("25")
        .arg("-g:none")
        .arg("-d")
        .arg(&classes)
        .arg(source)
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
async fn java_bytecode_calls_rust_and_missing_bindings_fail()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME to a Java 25 JDK and RUST_EDITION_JAVA25_HOME to the prepared Java 25 library home"
        );
        return Ok(());
    }
    let javac = javac().ok_or("JAVA_HOME must point to a Java 25 JDK with bin/javac")?;
    let java_home = java25_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let class_paths = [classes];

    let vm = JavaVm::new(
        &java_home,
        &class_paths,
        [
            NativeMethod::sync("NativeFixture.hostCall()I", native_host_call),
            NativeMethod::sync(
                "NativeFixture.ordinaryJava()I",
                callback_that_must_not_replace_bytecode,
            ),
        ],
    )
    .await?;
    assert!(
        vm.java_version().starts_with("25."),
        "{}",
        vm.java_version()
    );
    let result = vm
        .invoke("NativeFixture", "run()I", &[] as &[i32])
        .await?
        .ok_or("Java method returned no value")?;
    assert_eq!(result.as_i32()?, 42);

    let ordinary = vm
        .invoke("NativeFixture", "ordinaryJava()I", &[] as &[i32])
        .await?
        .ok_or("Java method returned no value")?;
    assert_eq!(ordinary.as_i32()?, 7);

    let no_binding =
        JavaVm::new(&java_home, &class_paths, std::iter::empty::<NativeMethod>()).await?;
    let error = no_binding
        .invoke("NativeFixture", "run()I", &[] as &[i32])
        .await
        .expect_err("an unregistered native method must fail");
    let message = error.to_string();
    assert!(message.contains("UnsatisfiedLinkError"), "{message}");

    Ok(())
}
