use rust_edition_java::{JavaVm, NativeMethod};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURE: &str = r#"
import java.nio.file.Files;
import java.nio.file.Path;

public final class CreateDirectoriesFixture {
    public static String create(String path) {
        try {
            Files.createDirectories(Path.of(path));
            return "created";
        } catch (Throwable error) {
            String message = String.valueOf(error.getMessage()).replace(path, "<target>");
            return error.getClass().getName() + ": " + message;
        }
    }

    public static void main(String[] args) {
        System.out.println(create(args[0]));
    }
}
"#;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let scratch_root = env::var_os("RUST_EDITION_JAVA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(".local/ristretto-evaluation")
            });
        let path = scratch_root.join(format!(
            "rust-edition-java-files-{}-{unique}",
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

fn java_tool(name: &str) -> Option<PathBuf> {
    let java_home = PathBuf::from(env::var_os("JAVA_HOME")?);
    #[cfg(windows)]
    let executable = format!("{name}.exe");
    #[cfg(not(windows))]
    let executable = name.to_string();
    let tool = java_home.join("bin").join(executable);
    if !tool.is_file() {
        return None;
    }
    if name == "javac" {
        let output = Command::new(&tool).arg("-version").output().ok()?;
        let version = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() || !version.contains("javac 25") {
            return None;
        }
    }
    Some(tool)
}

fn prepared_java25_home() -> Option<PathBuf> {
    env::var_os("RUST_EDITION_JAVA25_HOME").map(PathBuf::from)
}

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source = scratch.join("CreateDirectoriesFixture.java");
    let classes = scratch.join("classes");
    fs::create_dir_all(&classes)?;
    fs::write(&source, FIXTURE)?;
    let output = Command::new(javac)
        .args(["--release", "25", "-g:none", "-d"])
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

fn run_java(java: &Path, classes: &Path, path: &Path) -> std::io::Result<String> {
    let output = Command::new(java)
        .arg("-cp")
        .arg(classes)
        .arg("CreateDirectoriesFixture")
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Java 25 reference run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_vm(vm: &JavaVm, path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = path.to_str().ok_or("scratch path is not valid UTF-8")?;
    Ok(vm
        .invoke(
            "CreateDirectoriesFixture",
            "create(Ljava/lang/String;)Ljava/lang/String;",
            &[path],
        )
        .await?
        .ok_or("Java method returned no value")?
        .as_string()?)
}

fn record_result(failures: &mut Vec<String>, label: &str, java_result: &str, vm_result: &str) {
    if vm_result != java_result {
        failures.push(format!(
            "{label}: Java 25 returned {java_result:?}, Ristretto returned {vm_result:?}"
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_directories_matches_java_25_for_common_path_cases()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!("skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for Java 25 testing");
        return Ok(());
    }
    let javac = java_tool("javac").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java = java_tool("java").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java_home = prepared_java25_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let vm = JavaVm::new(
        &java_home,
        std::slice::from_ref(&classes),
        std::iter::empty::<NativeMethod>(),
    )
    .await?;

    let java_root = scratch.0.join("java");
    let vm_root = scratch.0.join("ristretto");
    fs::create_dir_all(&java_root)?;
    fs::create_dir_all(&vm_root)?;
    let mut failures = Vec::new();

    let java_one_level = java_root.join("one-level");
    let vm_one_level = vm_root.join("one-level");
    let java_result = run_java(&java, &classes, &java_one_level)?;
    let vm_result = run_vm(&vm, &vm_one_level).await?;
    record_result(
        &mut failures,
        "a single missing child",
        &java_result,
        &vm_result,
    );
    assert!(java_one_level.is_dir());
    if !vm_one_level.is_dir() {
        failures.push("Ristretto did not create a single missing child".to_string());
    }

    let java_parent = java_root.join("existing-parent");
    let vm_parent = vm_root.join("existing-parent");
    fs::create_dir_all(&java_parent)?;
    fs::create_dir_all(&vm_parent)?;
    let java_with_parent = java_parent.join(".fabric");
    let vm_with_parent = vm_parent.join(".fabric");
    let java_result = run_java(&java, &classes, &java_with_parent)?;
    let vm_result = run_vm(&vm, &vm_with_parent).await?;
    record_result(
        &mut failures,
        "a missing child under an existing parent",
        &java_result,
        &vm_result,
    );
    assert!(java_with_parent.is_dir());
    if !vm_with_parent.is_dir() {
        failures.push("Ristretto did not create a child under an existing parent".to_string());
    }

    let java_missing = java_root.join("game").join(".fabric");
    let vm_missing = vm_root.join("game").join(".fabric");
    let java_result = run_java(&java, &classes, &java_missing)?;
    let vm_result = run_vm(&vm, &vm_missing).await?;
    record_result(&mut failures, "a missing child", &java_result, &vm_result);
    assert!(java_missing.is_dir());
    if !vm_missing.is_dir() {
        failures.push("Ristretto did not create a missing child".to_string());
    }

    let java_nested = java_root.join("nested").join("one").join("two");
    let vm_nested = vm_root.join("nested").join("one").join("two");
    let java_result = run_java(&java, &classes, &java_nested)?;
    let vm_result = run_vm(&vm, &vm_nested).await?;
    record_result(
        &mut failures,
        "nested missing parents",
        &java_result,
        &vm_result,
    );
    assert!(java_nested.is_dir());
    if !vm_nested.is_dir() {
        failures.push("Ristretto did not create nested missing parents".to_string());
    }

    let java_existing = java_root.join("existing");
    let vm_existing = vm_root.join("existing");
    fs::create_dir_all(&java_existing)?;
    fs::create_dir_all(&vm_existing)?;
    let java_result = run_java(&java, &classes, &java_existing)?;
    let vm_result = run_vm(&vm, &vm_existing).await?;
    record_result(
        &mut failures,
        "an existing directory",
        &java_result,
        &vm_result,
    );
    assert_eq!(java_result, "created");

    let java_file = java_root.join("regular-file");
    let vm_file = vm_root.join("regular-file");
    fs::write(&java_file, b"file")?;
    fs::write(&vm_file, b"file")?;
    let java_result = run_java(&java, &classes, &java_file)?;
    let vm_result = run_vm(&vm, &vm_file).await?;
    record_result(
        &mut failures,
        "an existing regular file",
        &java_result,
        &vm_result,
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let java_real = java_root.join("real");
        let vm_real = vm_root.join("real");
        fs::create_dir_all(&java_real)?;
        fs::create_dir_all(&vm_real)?;
        let java_link = java_root.join("parent-link");
        let vm_link = vm_root.join("parent-link");
        symlink(&java_real, &java_link)?;
        symlink(&vm_real, &vm_link)?;
        let java_through_link = java_link.join("missing").join("child");
        let vm_through_link = vm_link.join("missing").join("child");
        let java_result = run_java(&java, &classes, &java_through_link)?;
        let vm_result = run_vm(&vm, &vm_through_link).await?;
        record_result(
            &mut failures,
            "a symlinked parent",
            &java_result,
            &vm_result,
        );
        assert!(java_real.join("missing/child").is_dir());
        if !vm_real.join("missing/child").is_dir() {
            failures.push(
                "Ristretto did not create directories through a symlinked parent".to_string(),
            );
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
