use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ristretto_vm::{Error, Parameters};
use rust_edition_java::{JavaVm, NativeMethod, NativeResult, Thread, Value};

const FIXTURE: &str = r#"
public final class HostStateFixture {
    public static native int add(int amount);
    public static native int read();
}
"#;

#[derive(Debug)]
struct Counter(AtomicI32);

#[allow(
    clippy::result_large_err,
    reason = "native callback helpers use Ristretto's required error type"
)]
fn counter(thread: &Arc<Thread>) -> NativeResult<Arc<Counter>> {
    let context = thread
        .vm()?
        .host_context()
        .ok_or_else(|| Error::InternalError("native callback has no host context".into()))?;
    Arc::downcast::<Counter>(context)
        .map_err(|_| Error::InternalError("native callback host context has the wrong type".into()))
}

#[allow(
    clippy::result_large_err,
    reason = "Ristretto's native callback signature requires its error type"
)]
fn add_counter(thread: Arc<Thread>, mut parameters: Parameters) -> NativeResult<Option<Value>> {
    let amount = parameters.pop_int()?;
    let value = counter(&thread)?.0.fetch_add(amount, Ordering::SeqCst) + amount;
    Ok(Some(Value::Int(value)))
}

#[allow(
    clippy::result_large_err,
    reason = "Ristretto's native callback signature requires its error type"
)]
fn read_counter(thread: Arc<Thread>, _parameters: Parameters) -> NativeResult<Option<Value>> {
    let value = counter(&thread)?.0.load(Ordering::SeqCst);
    Ok(Some(Value::Int(value)))
}

fn native_methods() -> [NativeMethod; 2] {
    [
        NativeMethod::sync("HostStateFixture.add(I)I", add_counter),
        NativeMethod::sync("HostStateFixture.read()I", read_counter),
    ]
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rust-edition-java-host-state-{}-{unique}",
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

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source = scratch.join("HostStateFixture.java");
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

fn java25_home() -> Option<PathBuf> {
    env::var_os("RUST_EDITION_JAVA25_HOME").map(PathBuf::from)
}

#[tokio::test(flavor = "multi_thread")]
async fn native_methods_use_their_own_vm_context_and_release_it_when_dropped()
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

    let first_state = Arc::new(Counter(AtomicI32::new(10)));
    let first_state_weak = Arc::downgrade(&first_state);
    let first_vm = JavaVm::new_with_host_context(
        &java_home,
        &class_paths,
        native_methods(),
        Arc::clone(&first_state),
    )
    .await?;
    assert!(Arc::ptr_eq(
        &first_state,
        &first_vm
            .host_context::<Counter>()
            .expect("first VM context")
    ));
    assert!(first_vm.host_context::<String>().is_none());

    let second_state = Arc::new(Counter(AtomicI32::new(100)));
    let second_state_weak = Arc::downgrade(&second_state);
    let second_vm = JavaVm::new_with_host_context(
        &java_home,
        &class_paths,
        native_methods(),
        Arc::clone(&second_state),
    )
    .await?;
    assert!(Arc::ptr_eq(
        &second_state,
        &second_vm
            .host_context::<Counter>()
            .expect("second VM context")
    ));

    drop(first_state);
    drop(second_state);

    let first_add = first_vm
        .invoke("HostStateFixture", "add(I)I", &[5_i32])
        .await?
        .expect("add returns a value");
    assert_eq!(first_add.as_i32()?, 15);
    let second_read = second_vm
        .invoke("HostStateFixture", "read()I", &[] as &[i32])
        .await?
        .expect("read returns a value");
    assert_eq!(second_read.as_i32()?, 100);

    let second_add = second_vm
        .invoke("HostStateFixture", "add(I)I", &[3_i32])
        .await?
        .expect("add returns a value");
    assert_eq!(second_add.as_i32()?, 103);
    let first_read = first_vm
        .invoke("HostStateFixture", "read()I", &[] as &[i32])
        .await?
        .expect("read returns a value");
    assert_eq!(first_read.as_i32()?, 15);

    drop(first_vm);
    assert!(first_state_weak.upgrade().is_none());
    assert!(second_state_weak.upgrade().is_some());

    drop(second_vm);
    assert!(second_state_weak.upgrade().is_none());
    Ok(())
}
