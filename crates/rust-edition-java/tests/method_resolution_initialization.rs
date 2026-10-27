use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_edition_java::{JavaVm, NativeMethod, Value};

const CLASS: &str = "dev.rustedition.fixture.ResolutionOrder";
const SOURCE: &str = r#"
package dev.rustedition.fixture;

public final class ResolutionOrder {
    private static int initialized;

    static final class VirtualTarget {
        static { initialized |= 1; }
        int value() { return 7; }
    }

    interface Contract {
        int MARK = mark(2);
        int value();
    }

    static class Parent {
        static { initialized |= 4; }
        static int inherited() { return 42; }
    }

    static final class Child extends Parent {
        static { initialized |= 8; }
    }

    private static int mark(int bit) {
        initialized |= bit;
        return bit;
    }

    public static int virtualNull() {
        try { ((VirtualTarget) null).value(); return -1; }
        catch (NullPointerException expected) { return initialized; }
    }

    public static int interfaceNull() {
        try { ((Contract) null).value(); return -1; }
        catch (NullPointerException expected) { return initialized; }
    }

    public static int inheritedStatic() {
        int result = Child.inherited();
        return result * 256 + initialized;
    }

    public static int newStillInitializes() {
        int result = new VirtualTarget().value();
        return result * 256 + initialized;
    }

    public static void main(String[] args) {
        System.out.println(virtualNull());
        System.out.println(interfaceNull());
        System.out.println(inheritedStatic());
        System.out.println(newStillInitializes());
    }
}
"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rust-edition-method-resolution-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tool(home: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    let name = format!("{name}.exe");
    home.join("bin").join(name)
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_instance_methods_does_not_start_their_classes()
-> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for the Java 25 comparison"
        );
        return Ok(());
    }
    let jdk = PathBuf::from(env::var_os("JAVA_HOME").ok_or("JAVA_HOME is required")?);
    let library_home = PathBuf::from(
        env::var_os("RUST_EDITION_JAVA25_HOME").ok_or("RUST_EDITION_JAVA25_HOME is required")?,
    );
    let javac = tool(&jdk, "javac");
    let version = Command::new(&javac).arg("-version").output()?;
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(version.status.success() && version_text.contains("javac 25"));

    let scratch = Scratch::new()?;
    let source = scratch.0.join("ResolutionOrder.java");
    let classes = scratch.0.join("classes");
    fs::create_dir(&classes)?;
    fs::write(&source, SOURCE)?;
    let compiled = Command::new(&javac)
        .args(["--release", "25", "-d"])
        .arg(&classes)
        .arg(&source)
        .output()?;
    assert!(
        compiled.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let reference = Command::new(tool(&jdk, "java"))
        .args(["--limit-modules", "java.base", "-cp"])
        .arg(&classes)
        .arg(CLASS)
        .output()?;
    assert!(
        reference.status.success(),
        "Java comparison failed: {}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected = String::from_utf8(reference.stdout)?
        .lines()
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()?;
    // No class startup for virtual/interface resolution; only Parent for its inherited
    // static method; normal construction still starts VirtualTarget afterward.
    assert_eq!(expected, [0, 0, 42 * 256 + 4, 7 * 256 + 5]);

    let vm = JavaVm::new(
        &library_home,
        &[classes],
        std::iter::empty::<NativeMethod>(),
    )
    .await?;
    for (method, expected) in [
        "virtualNull()I",
        "interfaceNull()I",
        "inheritedStatic()I",
        "newStillInitializes()I",
    ]
    .into_iter()
    .zip(expected)
    {
        let actual = vm
            .invoke(CLASS, method, &[] as &[Value])
            .await?
            .ok_or("fixture returned no value")?
            .as_i32()?;
        assert_eq!(actual, expected, "Java 25 differs for {method}");
    }
    Ok(())
}
