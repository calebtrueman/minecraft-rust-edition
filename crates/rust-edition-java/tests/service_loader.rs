use rust_edition_java::{JavaVm, NativeMethod};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURE: &str = r#"
import java.util.Iterator;
import java.util.ServiceLoader;

public final class ServiceLoaderProbe {
    public interface MarkerService {
        String name();
    }

    public static final class MarkerProvider implements MarkerService {
        public MarkerProvider() {}

        @Override
        public String name() {
            return "marker-provider";
        }
    }

    public static String firstProviderName() {
        Iterator<ServiceLoader.Provider<MarkerService>> providers =
            ServiceLoader.load(MarkerService.class).stream().iterator();
        if (!providers.hasNext()) {
            return "none";
        }
        MarkerService provider = providers.next().get();
        return provider.name() + ":" + provider.getClass().getName();
    }

    public static void main(String[] args) {
        System.out.print(firstProviderName());
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
            "rust-edition-java-service-loader-{}-{unique}",
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

fn java25_home() -> Option<PathBuf> {
    env::var_os("RUST_EDITION_JAVA25_HOME").map(PathBuf::from)
}

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source = scratch.join("ServiceLoaderProbe.java");
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

    let service_directory = classes.join("META-INF/services");
    fs::create_dir_all(&service_directory)?;
    fs::write(
        service_directory.join("ServiceLoaderProbe$MarkerService"),
        "ServiceLoaderProbe$MarkerProvider\n",
    )?;
    Ok(classes)
}

fn package_fixture(jar_tool: &Path, classes: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let jar = scratch.join("service-provider.jar");
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

fn run_stock_java(java: &Path, jar: &Path) -> std::io::Result<String> {
    let output = Command::new(java)
        .args(["--limit-modules", "java.base", "-cp"])
        .arg(jar)
        .arg("ServiceLoaderProbe")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Java 25 reference run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn service_loader_get_finds_a_provider_from_a_jar() -> Result<(), Box<dyn std::error::Error>>
{
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!("skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for Java 25 testing");
        return Ok(());
    }
    let javac = java_tool("javac").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java = java_tool("java").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let jar_tool = java_tool("jar").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java_home = java25_home().ok_or("RUST_EDITION_JAVA25_HOME must be set")?;

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let jar = package_fixture(&jar_tool, &classes, &scratch.0)?;

    let expected = run_stock_java(&java, &jar)?;
    assert_eq!(
        expected,
        "marker-provider:ServiceLoaderProbe$MarkerProvider"
    );

    let class_paths = [jar];
    let vm = JavaVm::new(&java_home, &class_paths, std::iter::empty::<NativeMethod>()).await?;
    let actual = vm
        .invoke(
            "ServiceLoaderProbe",
            "firstProviderName()Ljava/lang/String;",
            &[] as &[i32],
        )
        .await?
        .ok_or("Java service lookup returned no value")?
        .as_string()?;
    assert_eq!(actual, expected);
    Ok(())
}
