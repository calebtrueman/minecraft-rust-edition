use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use rust_edition_fabric::version_predicate::version_matches;
use serde_json::{Value, json};
use zip::ZipArchive;

const JAVA_ORACLE: &str = r#"
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.util.ArrayList;
import java.util.Collection;
import java.util.List;
import net.fabricmc.loader.api.Version;
import net.fabricmc.loader.api.VersionParsingException;
import net.fabricmc.loader.api.metadata.version.VersionPredicate;

public final class VersionPredicateOracle {
    public static void main(String[] args) throws Exception {
        BufferedReader input = new BufferedReader(new InputStreamReader(System.in));
        String line;
        while ((line = input.readLine()) != null) {
            String[] fields = line.split("\\t", -1);
            try {
                Version candidate = Version.parse(fields[1]);
                boolean matches;
                if (fields[0].equals("string")) {
                    matches = VersionPredicate.parse(fields[2]).test(candidate);
                } else if (fields[0].equals("array")) {
                    int count = Integer.parseInt(fields[2]);
                    List<String> ranges = new ArrayList<>();
                    for (int i = 0; i < count; i++) ranges.add(fields[i + 3]);
                    Collection<VersionPredicate> predicates = VersionPredicate.parse(ranges);
                    matches = predicates.stream().anyMatch(predicate -> predicate.test(candidate));
                } else {
                    throw new IllegalArgumentException("unknown rule type: " + fields[0]);
                }
                System.out.println(matches ? "MATCH" : "MISS");
            } catch (VersionParsingException error) {
                System.out.println("ERROR");
            }
        }
    }
}
"#;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "rust-edition-fabric-version-oracle-{}-{serial}",
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

fn java_tool(name: &str) -> Result<PathBuf, String> {
    let home = env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .ok_or("FABRIC_LOADER_JAR is set, so JAVA_HOME must point to a JDK")?;
    #[cfg(windows)]
    let executable = format!("{name}.exe");
    #[cfg(not(windows))]
    let executable = name.to_owned();
    let tool = home.join("bin").join(executable);
    if !tool.is_file() {
        return Err(format!("JAVA_HOME does not contain bin/{name}"));
    }
    Ok(tool)
}

fn check_loader_version(jar_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut archive = ZipArchive::new(File::open(jar_path)?)?;
    let mut manifest = String::new();
    archive
        .by_name("fabric.mod.json")?
        .read_to_string(&mut manifest)?;
    let metadata: Value = serde_json::from_str(&manifest)?;
    assert_eq!(
        metadata.get("version").and_then(Value::as_str),
        Some("0.19.5"),
        "FABRIC_LOADER_JAR must be Fabric Loader 0.19.5"
    );
    Ok(())
}

fn write_case_line(requirement: &Value, candidate: &str) -> String {
    match requirement {
        Value::String(range) => format!("string\t{candidate}\t{range}\n"),
        Value::Array(ranges) => {
            let mut line = format!("array\t{candidate}\t{}", ranges.len());
            for range in ranges {
                line.push('\t');
                line.push_str(range.as_str().expect("oracle cases use string ranges"));
            }
            line.push('\n');
            line
        }
        _ => panic!("oracle cases must use a string or string-array rule"),
    }
}

#[test]
fn fabric_loader_version_predicate_matches_installed_loader()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(jar_path) = env::var_os("FABRIC_LOADER_JAR").map(PathBuf::from) else {
        eprintln!("skipping installed-loader comparison: FABRIC_LOADER_JAR is not set");
        return Ok(());
    };
    if !jar_path.is_file() {
        return Err(format!("FABRIC_LOADER_JAR is not a file: {}", jar_path.display()).into());
    }
    check_loader_version(&jar_path)?;

    let javac = java_tool("javac")?;
    let java = java_tool("java")?;
    let scratch = ScratchDir::new()?;
    let classes = scratch.0.join("classes");
    fs::create_dir_all(&classes)?;
    let source = scratch.0.join("VersionPredicateOracle.java");
    fs::write(&source, JAVA_ORACLE)?;
    let compiled = Command::new(&javac)
        .args(["-encoding", "UTF-8", "-classpath"])
        .arg(&jar_path)
        .arg("-d")
        .arg(&classes)
        .arg(&source)
        .output()?;
    if !compiled.status.success() {
        return Err(format!(
            "could not compile the Fabric version oracle: {}",
            String::from_utf8_lossy(&compiled.stderr)
        )
        .into());
    }

    let cases = [
        (json!(">=1.2 <2"), "1.9", Some(true)),
        (json!(">=1.2 <2"), "2.0", Some(false)),
        (json!(["<1", ">=2 <3"]), "2.5", Some(true)),
        (json!(["<1", ">=2 <3"]), "1.5", Some(false)),
        (json!([]), "1.0", Some(false)),
        (json!(""), "odd-version-name", Some(true)),
        (json!("*"), "24w14a", Some(true)),
        (json!("26.1"), "26.1.0", Some(true)),
        (json!("=1.2.3+left"), "1.2.3+right", Some(true)),
        (json!(">=1.2.3+old"), "1.2.3+new", Some(true)),
        (json!(">1.2.3-alpha.9"), "1.2.3-alpha.10", Some(true)),
        (json!(">1.2.3-alpha.9"), "1.2.3-alpha.2", Some(false)),
        (json!("<1.2.3"), "1.2.3-rc.1", Some(true)),
        (json!(">1.2.3"), "1.2.3-rc.1", Some(false)),
        (json!(">1.0.0-alpha.9"), "1.0.0-alpha.09", Some(true)),
        (json!(">1.0.0-alpha.09"), "1.0.0-alpha.9", Some(false)),
        (json!("1.x"), "1.9.4", Some(true)),
        (json!("1.X"), "2.0", Some(false)),
        (json!("1.2.*"), "1.2.8", Some(true)),
        (json!("1.2.x"), "1.3.0", Some(false)),
        (json!("1.2.3.x"), "1.2.3.99", Some(true)),
        (json!("1.2.3.x"), "1.2.4.0", Some(false)),
        (json!("1.2.3.4.x"), "1.2.3.4.7", Some(true)),
        (json!("1.x"), "1.0.0-alpha.1", Some(true)),
        (json!("0.x"), "0.9.0", Some(true)),
        (json!("0.x"), "1.0.0", Some(false)),
        (json!("~1.2"), "1.2.99", Some(true)),
        (json!("~1.2"), "1.3.0", Some(false)),
        (json!("snapshot-a"), "snapshot-a", Some(true)),
        (json!("snapshot-a"), "snapshot-b", Some(false)),
        (json!(">=snapshot-a"), "snapshot-a", Some(true)),
        (json!(">=snapshot-a"), "snapshot-b", Some(false)),
        (json!(">=1.0"), "release-1.0", Some(false)),
        (json!("release-1.0"), "release-1.0", Some(true)),
        (json!("1.2.x-rc.1"), "1.2.x-rc.1", Some(true)),
        (json!("1.2.x-rc.1"), "1.2.3-rc.1", Some(false)),
        (json!("1.x.2"), "1.x.2", Some(true)),
        (json!(">snapshot"), "snapshot", None),
        (json!(">=1.x"), "1.2", None),
        (json!("^0.x"), "0.9.0", None),
    ];

    let mut input_text = String::new();
    for (requirement, candidate, expected) in &cases {
        let rust_result = version_matches(requirement, candidate);
        match (rust_result, *expected) {
            (Ok(actual), Some(expected)) => {
                assert_eq!(actual, expected, "{requirement} vs {candidate}")
            }
            (Err(_), None) => {}
            (Ok(actual), None) => panic!("Rust accepted malformed rule {requirement}: {actual}"),
            (Err(error), Some(_)) => panic!("Rust rejected {requirement}: {error}"),
        }
        input_text.push_str(&write_case_line(requirement, candidate));
    }

    let class_path = env::join_paths([jar_path.as_os_str(), classes.as_os_str()])?;
    let mut child = Command::new(java)
        .arg("-classpath")
        .arg(class_path)
        .arg("VersionPredicateOracle")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("could not open Java oracle input")?
        .write_all(input_text.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "Fabric Loader oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let actual = String::from_utf8(output.stdout)?;
    let actual = actual.lines().collect::<Vec<_>>();
    assert_eq!(
        actual.len(),
        cases.len(),
        "oracle returned the wrong number of results"
    );
    for ((requirement, candidate, expected), result) in cases.iter().zip(actual) {
        let expected = expected.map_or("ERROR", |matches| if matches { "MATCH" } else { "MISS" });
        assert_eq!(
            result, expected,
            "Fabric Loader {requirement} vs {candidate}"
        );
    }
    Ok(())
}
