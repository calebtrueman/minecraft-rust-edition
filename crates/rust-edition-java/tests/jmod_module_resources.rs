use rust_edition_java::{JavaVm, NativeMethod};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURE: &str = r#"
import java.io.InputStream;
import java.text.Normalizer;

public final class JmodModuleResourceFixture {
    public static String normalize() {
        return Normalizer.normalize("\u212B", Normalizer.Form.NFKD);
    }

    public static String stringModuleName() {
        return String.valueOf(String.class.getModule().getName());
    }

    public static String icuModuleName() throws ClassNotFoundException {
        Class<?> icuBinary = Class.forName("jdk.internal.icu.impl.ICUBinary");
        return String.valueOf(icuBinary.getModule().getName());
    }

    public static String hiddenIcuResource() {
        try (InputStream input = String.class.getResourceAsStream(
                "/jdk/internal/icu/impl/data/icudt76b/nfkc.nrm")) {
            return input == null ? "hidden" : "visible";
        } catch (Exception error) {
            return "error: " + error.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println(normalize());
        System.out.println(stringModuleName());
        System.out.println(icuModuleName());
        System.out.println(hiddenIcuResource());
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
        let path = env::temp_dir().join(format!(
            "rust-edition-java-jmod-{}-{unique}",
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

fn java25_tool(name: &str) -> Option<PathBuf> {
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
    let source = scratch.join("JmodModuleResourceFixture.java");
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

fn java_reference(java: &Path, classes: &Path) -> std::io::Result<Vec<String>> {
    let output = Command::new(java)
        .arg("-cp")
        .arg(classes)
        .arg("JmodModuleResourceFixture")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Java 25 reference run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

async fn invoke_string(vm: &JavaVm, method: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = vm
        .invoke("JmodModuleResourceFixture", method, &[] as &[i32])
        .await?
        .ok_or("Java method returned no value")?;
    Ok(value.as_string()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn jmod_home_keeps_java_base_resources_and_module_rules()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!("skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for Java 25 testing");
        return Ok(());
    }
    let javac = java25_tool("javac").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java = java25_tool("java").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java_home = prepared_java25_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;
    if java_home.join("lib/modules").is_file() {
        return Err("RUST_EDITION_JAVA25_HOME must be the prepared JMOD-only home".into());
    }

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let reference = java_reference(&java, &classes)?;
    assert_eq!(reference.len(), 4, "unexpected Java 25 reference output");
    assert_eq!(reference[0], "A\u{30A}");
    assert_eq!(reference[1], "java.base");
    assert_eq!(reference[2], "java.base");
    assert_eq!(reference[3], "hidden");

    let vm = JavaVm::new(&java_home, &[classes], std::iter::empty::<NativeMethod>()).await?;
    assert_eq!(
        invoke_string(&vm, "normalize()Ljava/lang/String;").await?,
        reference[0]
    );
    assert_eq!(
        invoke_string(&vm, "stringModuleName()Ljava/lang/String;").await?,
        reference[1]
    );
    assert_eq!(
        invoke_string(&vm, "icuModuleName()Ljava/lang/String;").await?,
        reference[2]
    );
    assert_eq!(
        invoke_string(&vm, "hiddenIcuResource()Ljava/lang/String;").await?,
        reference[3]
    );

    Ok(())
}
