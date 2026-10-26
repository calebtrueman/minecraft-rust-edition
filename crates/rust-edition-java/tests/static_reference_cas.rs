use rust_edition_java::{JavaVm, NativeMethod};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURE: &str = r#"
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public final class StaticReferenceCasFixture {
    public static int holderInitializations;
    public static int initCountDuringCas;
    public static boolean lazyHandle;
    public static boolean wrongObjectWhileNull;
    public static boolean nullToNull;
    public static boolean nullToObject;
    public static boolean equalButDifferentObject;
    public static boolean sameObject;
    public static boolean storedMarker;
    public static boolean objectToNull;
    public static boolean storedNull;

    public static final class EqualBox {
        public int value;

        public EqualBox(int value) {
            this.value = value;
        }
    }

    public static final class Holder {
        public static Object value;

        static {
            holderInitializations++;
            try {
                VarHandle handle = MethodHandles.lookup().findStaticVarHandle(
                    Holder.class, "value", Object.class);
                lazyHandle = handle.getClass().getName().equals(
                    "java.lang.invoke.LazyInitializingVarHandle");
                initCountDuringCas = holderInitializations;

                wrongObjectWhileNull = handle.compareAndSet(new EqualBox(7), new Object());
                nullToNull = handle.compareAndSet(null, null);
                EqualBox current = new EqualBox(7);
                nullToObject = handle.compareAndSet(null, current);
                equalButDifferentObject = handle.compareAndSet(new EqualBox(7), new Object());
                Object marker = new Object();
                sameObject = handle.compareAndSet(current, marker);
                storedMarker = handle.get() == marker && Holder.value == marker;
                objectToNull = handle.compareAndSet(marker, null);
                storedNull = handle.get() == null && Holder.value == null;
            } catch (Throwable error) {
                throw new ExceptionInInitializerError(error);
            }
        }
    }

    public static int run() {
        int before = holderInitializations;
        Object first = Holder.value;
        int afterFirst = holderInitializations;
        Object second = Holder.value;
        int afterSecond = holderInitializations;

        int result = 0;
        if (lazyHandle) result |= 1 << 0;
        if (before == 0) result |= 1 << 1;
        if (afterFirst == 1) result |= 1 << 2;
        if (afterSecond == 1) result |= 1 << 3;
        if (initCountDuringCas == 1) result |= 1 << 4;
        if (wrongObjectWhileNull) result |= 1 << 5;
        if (nullToNull) result |= 1 << 6;
        if (nullToObject) result |= 1 << 7;
        if (equalButDifferentObject) result |= 1 << 8;
        if (sameObject) result |= 1 << 9;
        if (storedMarker && first == second) result |= 1 << 10;
        if (objectToNull) result |= 1 << 11;
        if (storedNull) result |= 1 << 12;
        return result;
    }

    public static void main(String[] args) {
        System.out.println(run());
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
        let root = env::var_os("RUST_EDITION_JAVA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(".local/ristretto-evaluation")
            });
        let path = root.join(format!(
            "rust-edition-java-static-reference-cas-{}-{unique}",
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

fn compile_fixture(javac: &Path, scratch: &Path) -> std::io::Result<PathBuf> {
    let source = scratch.join("StaticReferenceCasFixture.java");
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

fn run_java(java: &Path, classes: &Path) -> std::io::Result<i32> {
    let output = Command::new(java)
        .args(["--limit-modules", "java.base", "-cp"])
        .arg(classes)
        .arg("StaticReferenceCasFixture")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Java 25 reference run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(std::io::Error::other)
}

async fn run_vm(vm: &JavaVm) -> Result<i32, Box<dyn std::error::Error>> {
    Ok(vm
        .invoke("StaticReferenceCasFixture", "run()I", &[] as &[i32])
        .await?
        .ok_or("Java method returned no value")?
        .as_i32()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn static_reference_cas_matches_java_25_during_holder_initialization()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!("skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for Java 25 testing");
        return Ok(());
    }
    let javac = java_tool("javac").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java = java_tool("java").ok_or("JAVA_HOME must point to a Java 25 JDK")?;
    let java_home = env::var_os("RUST_EDITION_JAVA25_HOME")
        .map(PathBuf::from)
        .ok_or("RUST_EDITION_JAVA25_HOME must point to the Java 25 runtime")?;

    let scratch = ScratchDir::new()?;
    let classes = compile_fixture(&javac, &scratch.0)?;
    let vm = JavaVm::new(
        &java_home,
        std::slice::from_ref(&classes),
        std::iter::empty::<NativeMethod>(),
    )
    .await?;

    let java_result = run_java(&java, &classes)?;
    let vm_result = run_vm(&vm).await?;
    let expected = 7_903;
    assert_eq!(java_result, expected, "Java 25 fixture expectation changed");
    assert_eq!(vm_result, java_result);
    Ok(())
}
