use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rust_edition_java::{JavaVm, NativeMethod, Value};

const CLASS: &str = "dev.rustedition.fixture.ByteArrayProbe";
const CHILD_CLASSES: &str = "RUST_EDITION_BYTE_ARRAY_HANDLE_TEST_CLASSES";
const CHILD_EXPECTED: &str = "RUST_EDITION_BYTE_ARRAY_HANDLE_TEST_EXPECTED";
const SOURCE: &str = r#"
package dev.rustedition.fixture;

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

public final class ByteArrayProbe {
    private static final byte[] DATA = {1, 0x23, 0x45, 0x67, (byte)0x89, (byte)0xab, (byte)0xcd, (byte)0xef, 0x11};

    public static long read(int kind, int little, int offset) {
        ByteOrder order = little == 0 ? ByteOrder.BIG_ENDIAN : ByteOrder.LITTLE_ENDIAN;
        switch (kind) {
            case 0: return (short) MethodHandles.byteArrayViewVarHandle(short[].class, order).get(DATA, offset);
            case 1: return (char) MethodHandles.byteArrayViewVarHandle(char[].class, order).get(DATA, offset);
            case 2: return (int) MethodHandles.byteArrayViewVarHandle(int[].class, order).get(DATA, offset);
            case 3: return (long) MethodHandles.byteArrayViewVarHandle(long[].class, order).get(DATA, offset);
            case 4: return Float.floatToRawIntBits((float) MethodHandles.byteArrayViewVarHandle(float[].class, order).get(DATA, offset));
            case 5: return Double.doubleToRawLongBits((double) MethodHandles.byteArrayViewVarHandle(double[].class, order).get(DATA, offset));
            default: throw new AssertionError("unknown kind");
        }
    }

    public static long errors() {
        VarHandle handle = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN);
        int caught = 0;
        try { long value = (long) handle.get((byte[])null, 0); }
        catch (NullPointerException expected) { caught |= 1; }
        try { long value = (long) handle.get(DATA, -1); }
        catch (IndexOutOfBoundsException expected) {
            if (expected.getClass() != ArrayIndexOutOfBoundsException.class) throw expected;
            caught |= 2;
        }
        try { long value = (long) handle.get(DATA, 2); }
        catch (IndexOutOfBoundsException expected) {
            if (expected.getClass() != ArrayIndexOutOfBoundsException.class) throw expected;
            caught |= 4;
        }
        try { long value = (long) handle.get(new byte[7], 0); }
        catch (IndexOutOfBoundsException expected) {
            if (expected.getClass() != ArrayIndexOutOfBoundsException.class) throw expected;
            caught |= 8;
        }
        return caught;
    }

    public static long negativeShort() {
        return (short) MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN).get(DATA, 4);
    }
    public static long unsignedChar() {
        return (char) MethodHandles.byteArrayViewVarHandle(char[].class, ByteOrder.BIG_ENDIAN).get(DATA, 4);
    }
    public static long floatNanBits() {
        byte[] bytes = {0x7f, (byte)0xc0, 0x12, 0x34};
        return Float.floatToRawIntBits((float) MethodHandles.byteArrayViewVarHandle(float[].class, ByteOrder.BIG_ENDIAN).get(bytes, 0));
    }
    public static long doubleNanBits() {
        byte[] bytes = {(byte)0xff, (byte)0xf8, 0x12, 0x34, 0x56, 0x78, (byte)0x9a, (byte)0xbc};
        return Double.doubleToRawLongBits((double) MethodHandles.byteArrayViewVarHandle(double[].class, ByteOrder.BIG_ENDIAN).get(bytes, 0));
    }

    public static void main(String[] args) {
        for (int kind = 0; kind < 6; kind++) {
            for (int little = 0; little < 2; little++) {
                for (int offset = 0; offset < 2; offset++) System.out.println(read(kind, little, offset));
            }
        }
        System.out.println(errors());
        System.out.println(negativeShort());
        System.out.println(unsignedChar());
        System.out.println(floatNanBits());
        System.out.println(doubleNanBits());
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
            "rust-edition-byte-array-handle-{}-{stamp}",
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

#[test]
fn byte_array_field_handles_match_java_25() -> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("JAVA_HOME").is_none() && env::var_os("RUST_EDITION_JAVA25_HOME").is_none() {
        eprintln!(
            "skipping: set JAVA_HOME and RUST_EDITION_JAVA25_HOME for the Java 25 comparison"
        );
        return Ok(());
    }
    let jdk = PathBuf::from(env::var_os("JAVA_HOME").ok_or("JAVA_HOME is required")?);
    let home =
        env::var_os("RUST_EDITION_JAVA25_HOME").ok_or("RUST_EDITION_JAVA25_HOME is required")?;
    let javac = tool(&jdk, "javac");
    let version = Command::new(&javac).arg("-version").output()?;
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(version.status.success() && version_text.contains("javac 25"));
    let scratch = Scratch::new()?;
    let classes = scratch.0.join("classes");
    fs::create_dir(&classes)?;
    let source = scratch.0.join("ByteArrayProbe.java");
    fs::write(&source, SOURCE)?;
    let compiled = Command::new(javac)
        .args(["--release", "25", "-d"])
        .arg(&classes)
        .arg(&source)
        .output()?;
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let reference = Command::new(tool(&jdk, "java"))
        .args(["--limit-modules", "java.base", "-cp"])
        .arg(&classes)
        .arg(CLASS)
        .output()?;
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected = String::from_utf8(reference.stdout)?
        .lines()
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(expected.len(), 29);
    assert_eq!(expected[0], 0x0123);
    assert_eq!(expected[24], 15);
    assert_eq!(expected[25], -30293);
    assert_eq!(expected[26], 35243);
    assert_eq!(expected[27], 0x7fc01234);
    assert_eq!(expected[28], 0xfff8123456789abc_u64 as i64);

    // Run the VM in a child so a regression in VarHandle's recursive fallback has a firm
    // deadline, rather than leaving the test runner stuck in the interpreter.
    let mut child = Command::new(env::current_exe()?)
        .args(["--exact", "byte_array_handle_child", "--nocapture"])
        .env(CHILD_CLASSES, &classes)
        .env(
            CHILD_EXPECTED,
            expected
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .env("RUST_EDITION_JAVA25_HOME", home)
        .env("RUST_BACKTRACE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut timed_out = false;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            timed_out = true;
            child.kill()?;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output()?;
    assert!(
        !timed_out && output.status.success(),
        "Rust VarHandle comparison failed (timed_out={timed_out}):\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn byte_array_handle_child() -> Result<(), Box<dyn std::error::Error>> {
    let Some(classes) = env::var_os(CHILD_CLASSES) else {
        return Ok(());
    };
    let expected = env::var(CHILD_EXPECTED)?
        .split(',')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(expected.len(), 29);
    let home =
        PathBuf::from(env::var_os("RUST_EDITION_JAVA25_HOME").ok_or("missing Java library home")?);
    let vm = JavaVm::new(
        &home,
        &[PathBuf::from(classes)],
        std::iter::empty::<NativeMethod>(),
    )
    .await?;
    let mut index = 0;
    for kind in 0..6 {
        for little in 0..2 {
            for offset in 0..2 {
                let actual = vm
                    .invoke(CLASS, "read(III)J", &[kind, little, offset])
                    .await?
                    .ok_or("fixture returned no value")?
                    .as_i64()?;
                assert_eq!(
                    actual, expected[index],
                    "kind={kind}, little={little}, offset={offset}"
                );
                index += 1;
            }
        }
    }
    for method in [
        "errors()J",
        "negativeShort()J",
        "unsignedChar()J",
        "floatNanBits()J",
        "doubleNanBits()J",
    ] {
        let actual = vm
            .invoke(CLASS, method, &[] as &[Value])
            .await?
            .ok_or("fixture returned no value")?
            .as_i64()?;
        assert_eq!(actual, expected[index], "Java 25 differs for {method}");
        index += 1;
    }
    Ok(())
}
