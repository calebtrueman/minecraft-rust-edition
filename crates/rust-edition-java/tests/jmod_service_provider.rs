use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_edition_java::{Error, JavaVm, NativeMethod};

const CLASS: &str = "dev.rustedition.fixture.ZipProvider";
const SOURCE: &str = r#"
package dev.rustedition.fixture;

import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.spi.FileSystemProvider;
import java.util.Map;

public final class ZipProvider {
    public static String run(String archivePath) throws Exception {
        FileSystemProvider jarProvider = null;
        for (FileSystemProvider provider : FileSystemProvider.installedProviders()) {
            if ("jar".equals(provider.getScheme())) jarProvider = provider;
        }
        if (jarProvider == null) throw new AssertionError("jar provider was not discovered");
        Path archive = Path.of(archivePath);
        String text = "zipfs works";
        try (FileSystem zip = FileSystems.newFileSystem(
                URI.create("jar:" + archive.toUri()), Map.of("create", "true"))) {
            Path entry = zip.getPath("/folder/result.txt");
            Files.createDirectory(entry.getParent());
            Files.writeString(entry, text);
            if (!text.equals(Files.readString(entry))) throw new AssertionError("write/read differs");
        }
        try (FileSystem zip = FileSystems.newFileSystem(archive)) {
            if (!text.equals(Files.readString(zip.getPath("/folder/result.txt")))) {
                throw new AssertionError("reopening the ZIP changed its contents");
            }
        }
        return jarProvider.getClass().getModule().getName() + "|" + text;
    }

    public static void main(String[] args) throws Exception {
        System.out.println(run(args[0]));
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
            "rust edition zip provider-{}-{stamp}",
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

async fn describe_failure(vm: &JavaVm, error: &Error) -> String {
    let Error::Ristretto(inner) = error else {
        return error.to_string();
    };
    let ristretto_vm::Error::Throwable(throwable) = inner.as_ref() else {
        return error.to_string();
    };
    let mut current = throwable.clone();
    let mut messages = Vec::new();
    for _ in 0..8 {
        if let Ok(Some(text)) = vm
            .invoke(
                "java.lang.Throwable",
                "toString()Ljava/lang/String;",
                std::slice::from_ref(&current),
            )
            .await
            && let Ok(text) = text.as_string()
        {
            messages.push(text.chars().take(1024).collect::<String>());
        }
        let Ok(Some(cause)) = vm
            .invoke(
                "java.lang.Throwable",
                "getCause()Ljava/lang/Throwable;",
                std::slice::from_ref(&current),
            )
            .await
        else {
            break;
        };
        if cause.is_null() || vm.same_object(&cause, &current) {
            break;
        }
        current = cause;
    }
    messages.join(" caused by ")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jmod_service_provider_can_create_and_reopen_a_zip_filesystem()
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
    if !library_home.join("jmods/jdk.zipfs.jmod").is_file() {
        eprintln!("skipping: this comparison needs jdk.zipfs in the prepared Java home");
        return Ok(());
    }
    let javac = tool(&jdk, "javac");
    let version = Command::new(&javac).arg("-version").output()?;
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(version.status.success() && version_text.contains("javac 25"));

    let scratch = Scratch::new()?;
    let source = scratch.0.join("ZipProvider.java");
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
        .args(["--limit-modules", "java.base,jdk.zipfs", "-cp"])
        .arg(&classes)
        .arg(CLASS)
        .arg(scratch.0.join("java reference.zip"))
        .output()?;
    assert!(
        reference.status.success(),
        "Java comparison failed: {}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected = String::from_utf8(reference.stdout)?.trim().to_owned();
    assert_eq!(expected, "jdk.zipfs|zipfs works");

    let vm = JavaVm::new(
        &library_home,
        &[classes],
        std::iter::empty::<NativeMethod>(),
    )
    .await?;
    let rust_archive = scratch.0.join("rust helper.zip");
    let rust_archive_string = rust_archive.to_str().ok_or("test path is not UTF-8")?;
    let result = vm
        .invoke(
            CLASS,
            "run(Ljava/lang/String;)Ljava/lang/String;",
            &[rust_archive_string],
        )
        .await;
    let actual = match result {
        Ok(value) => value,
        Err(error) => panic!(
            "Rust ZIP provider failed: {}",
            describe_failure(&vm, &error).await
        ),
    }
    .ok_or("fixture returned no value")?
    .as_string()?;
    assert_eq!(actual, expected);
    assert!(rust_archive.is_file());
    Ok(())
}
