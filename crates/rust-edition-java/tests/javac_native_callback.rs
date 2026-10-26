use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_edition_java::{JavaVm, NativeMethod, NativeResult, Parameters, Thread, Value};

const FIXTURE: &str = r#"
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;

public final class NativeFixture {
    public static native int hostCall();

    public static int run() {
        return hostCall();
    }

    public static int ordinaryJava() {
        return 7;
    }

    public static int codeSourcePathKind() throws Exception {
        URI location = NativeFixture.class.getProtectionDomain().getCodeSource().getLocation().toURI();
        Path path = Path.of(location);
        if (!Files.exists(path)) {
            throw new IllegalStateException("decoded code-source path does not exist: " + path);
        }
        if (Files.isDirectory(path)) {
            if (!Files.isRegularFile(path.resolve("NativeFixture.class"))) {
                throw new IllegalStateException("decoded class directory is wrong: " + path);
            }
            return 1;
        }
        if (Files.isRegularFile(path) && path.getFileName().toString().endsWith(".jar")) {
            return 2;
        }
        throw new IllegalStateException("decoded code-source path is not a directory or JAR: " + path);
    }

    public static void main(String[] args) throws Exception {
        System.out.print(codeSourcePathKind());
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
        let path = env::temp_dir().join(format!(
            "rust edition java-雪-#-%-{}-{unique}",
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

fn java() -> Option<PathBuf> {
    let java_home = PathBuf::from(env::var_os("JAVA_HOME")?);
    #[cfg(windows)]
    let binary = "java.exe";
    #[cfg(not(windows))]
    let binary = "java";
    let java = java_home.join("bin").join(binary);
    java.is_file().then_some(java)
}

#[cfg(windows)]
const JAR_TOOL: &str = "jar.exe";
#[cfg(not(windows))]
const JAR_TOOL: &str = "jar";

fn package_fixture(jar_tool: &Path, classes: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let jar = scratch.join("fixture archive-雪-#-%.jar");
    let output = Command::new(jar_tool)
        .arg("--create")
        .arg("--file")
        .arg(&jar)
        .arg("-C")
        .arg(classes)
        .arg(".")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "jar tool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(jar)
}

fn run_with_java(java: &Path, class_path: &Path) -> std::io::Result<String> {
    let output = Command::new(java)
        .arg("-cp")
        .arg(class_path)
        .arg("NativeFixture")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Java 25 fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
async fn java_bytecode_calls_rust_from_paths_with_url_characters_and_missing_bindings_fail()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME to a Java 25 JDK and RUST_EDITION_JAVA25_HOME to the prepared Java 25 library home"
        );
        return Ok(());
    }
    let javac = javac().ok_or("JAVA_HOME must point to a Java 25 JDK with bin/javac")?;
    let java = java().ok_or("JAVA_HOME must point to a Java 25 JDK with bin/java")?;
    let java_home = java25_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let java_home_path = PathBuf::from(env::var_os("JAVA_HOME").ok_or("JAVA_HOME must be set")?);
    let jar_tool = java_home_path.join("bin").join(JAR_TOOL);
    if !jar_tool.is_file() {
        return Err("JAVA_HOME must point to a Java 25 JDK with bin/jar".into());
    }
    let jar = package_fixture(&jar_tool, &classes, &scratch.0)?;

    let native_methods = || {
        [
            NativeMethod::sync("NativeFixture.hostCall()I", native_host_call),
            NativeMethod::sync(
                "NativeFixture.ordinaryJava()I",
                callback_that_must_not_replace_bytecode,
            ),
        ]
    };

    for (class_path, expected_kind, label) in
        [(&classes, 1, "classes directory"), (&jar, 2, "JAR file")]
    {
        assert_eq!(
            run_with_java(&java, class_path)?,
            expected_kind.to_string(),
            "Java 25 should resolve the {label} code-source URI"
        );

        let class_paths = [class_path.to_path_buf()];
        let vm = JavaVm::new(&java_home, &class_paths, native_methods()).await?;
        assert!(vm.host_context::<String>().is_none());
        assert!(
            vm.java_version().starts_with("25."),
            "{}",
            vm.java_version()
        );

        let path_kind = vm
            .invoke("NativeFixture", "codeSourcePathKind()I", &[] as &[i32])
            .await?
            .ok_or("Java method returned no code-source path kind")?;
        assert_eq!(
            path_kind.as_i32()?,
            expected_kind,
            "Ristretto should decode the {label} code-source URI into an existing path"
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
    }

    let class_paths = [classes];
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
