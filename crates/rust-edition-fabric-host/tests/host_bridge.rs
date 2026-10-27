use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rust_edition_mod_bridge::{
    Bridge, BridgeError, BridgeStatus, ClockSnapshot, MAX_VALUE_BYTES, PlayerSnapshot,
    ResultStatus, Snapshot, SpawnConfig, snapshot_json,
};
use serde_json::json;

const OPT_IN: &str = "RUST_EDITION_FABRIC_HOST_TEST";
const TEST_MOD: &str = r#"
package dev.rustedition.probe;

import net.fabricmc.api.ModInitializer;

public final class TestMod implements ModInitializer, SnapshotTick {
    public static native int hostCall();

    @Override
    public void onInitialize() {
        System.setProperty("probe.initialized", Integer.toString(hostCall()));
    }

    @Override
    public String onSnapshot(String snapshotJson) {
        return Integer.toString(snapshotJson.hashCode() + hostCall());
    }
}
"#;

const SNAPSHOT_TICK: &str = r#"
package dev.rustedition.probe;

public interface SnapshotTick {
    String onSnapshot(String snapshotJson);
}
"#;

const CAUSE_FAILURE_MOD: &str = r#"
package dev.rustedition.probe;

import net.fabricmc.api.ModInitializer;

public final class CauseFailureMod implements ModInitializer {
    public CauseFailureMod() {
        throw new RuntimeException("fixture wrapper", new IllegalArgumentException("fixture root cause"));
    }

    @Override public void onInitialize() {}
}
"#;

const CYCLE_FAILURE_MOD: &str = r#"
package dev.rustedition.probe;

import net.fabricmc.api.ModInitializer;

public final class CycleFailureMod implements ModInitializer {
    public CycleFailureMod() {
        Throwable first = new RuntimeException("cycle first");
        Throwable second = new RuntimeException("cycle second");
        first.initCause(second);
        second.initCause(first);
        throw (RuntimeException) first;
    }

    @Override public void onInitialize() {}
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
            "rust edition fabric host-{}-{unique}",
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

fn required_path(name: &str, is_directory: bool) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let value = env::var_os(name).ok_or_else(|| format!("set {name} before opting in"))?;
    let path = fs::canonicalize(PathBuf::from(value))?;
    if (is_directory && !path.is_dir()) || (!is_directory && !path.is_file()) {
        return Err(format!("{name} has the wrong file type: {}", path.display()).into());
    }
    Ok(path)
}

fn java_tool(java_home: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    let name = format!("{name}.exe");
    #[cfg(not(windows))]
    let name = name.to_owned();
    java_home.join("bin").join(name)
}

fn package_mod(
    jar_tool: &Path,
    jar_path: &Path,
    classes: &Path,
    metadata_dir: &Path,
) -> std::io::Result<()> {
    let output = Command::new(jar_tool)
        .arg("--create")
        .arg("--file")
        .arg(jar_path)
        .arg("-C")
        .arg(classes)
        .arg(".")
        .arg("-C")
        .arg(metadata_dir)
        .arg("fabric.mod.json")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "jar failed to package a test mod: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn expect_startup_failure(
    helper: &Path,
    config: &Path,
    scratch: &Path,
    expected: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    fs::create_dir_all(scratch)?;
    let bridge = Bridge::spawn(SpawnConfig::new(helper, config, scratch))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match bridge.status() {
            BridgeStatus::Failed(BridgeError::HelperFailed(message)) => {
                assert!(
                    message.len() <= MAX_VALUE_BYTES,
                    "startup failure must fit the bridge's {MAX_VALUE_BYTES}-byte limit"
                );
                assert!(
                    message.contains(expected),
                    "startup failure did not include {expected:?}: {message}"
                );
                return Ok(message);
            }
            BridgeStatus::Failed(error) => {
                return Err(format!("unexpected bridge failure: {error}").into());
            }
            BridgeStatus::Ready => {
                return Err("the deliberately failing Fabric mod unexpectedly became ready".into());
            }
            BridgeStatus::Stopped => {
                return Err("the failing Fabric helper stopped without a failure message".into());
            }
            BridgeStatus::Starting if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            BridgeStatus::Starting => {
                return Err(
                    "the failing Fabric helper did not report an error in 60 seconds".into(),
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_runs_fabric_and_returns_the_result_for_the_exact_bridge_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(opt_in) = env::var_os(OPT_IN) else {
        eprintln!(
            "skipping: set {OPT_IN}=1 and all Java 25, Fabric, Mixin, ASM, and Minecraft paths"
        );
        return Ok(());
    };
    if opt_in != "1" {
        return Err(format!("{OPT_IN} must be exactly 1 when set").into());
    }

    let jdk_home = required_path("JAVA_HOME", true)?;
    let java_home = required_path("RUST_EDITION_JAVA25_HOME", true)?;
    let loader = required_path("RUST_EDITION_JAVA_FABRIC_LOADER_JAR", false)?;
    let game = required_path("RUST_EDITION_JAVA_FABRIC_GAME_JAR", false)?;
    let asm = required_path("RUST_EDITION_JAVA_FABRIC_ASM_JAR", false)?;
    let asm_analysis = required_path("RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR", false)?;
    let asm_commons = required_path("RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR", false)?;
    let asm_tree = required_path("RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR", false)?;
    let asm_util = required_path("RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR", false)?;
    let mixin = required_path("RUST_EDITION_JAVA_FABRIC_MIXIN_JAR", false)?;
    let javac = java_tool(&jdk_home, "javac");
    let jar = java_tool(&jdk_home, "jar");
    if !javac.is_file() || !jar.is_file() {
        return Err("JAVA_HOME must include javac and jar for this opt-in test".into());
    }

    let scratch = ScratchDir::new()?;
    let classes = scratch.0.join("classes");
    let mod_jars = scratch.0.join("mods");
    fs::create_dir_all(&classes)?;
    fs::create_dir_all(&mod_jars)?;
    let mod_source = scratch.0.join("TestMod.java");
    let tick_source = scratch.0.join("SnapshotTick.java");
    fs::write(&mod_source, TEST_MOD)?;
    fs::write(&tick_source, SNAPSHOT_TICK)?;
    let compile = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(&loader)
        .arg("-d")
        .arg(&classes)
        .arg(&mod_source)
        .arg(&tick_source)
        .output()?;
    if !compile.status.success() {
        return Err(format!(
            "javac failed to build the tiny test mod: {}",
            String::from_utf8_lossy(&compile.stderr)
        )
        .into());
    }
    let metadata = scratch.0.join("fabric.mod.json");
    fs::write(
        &metadata,
        r#"{"schemaVersion":1,"id":"probe_mod","version":"1.0.0","entrypoints":{"main":["dev.rustedition.probe.TestMod"]},"depends":{"fabricloader":">=0.19.5","minecraft":"26.3"}}"#,
    )?;
    let mod_jar = mod_jars.join("probe-mod.jar");
    let package = Command::new(&jar)
        .arg("--create")
        .arg("--file")
        .arg(&mod_jar)
        .arg("-C")
        .arg(&classes)
        .arg(".")
        .arg("-C")
        .arg(&scratch.0)
        .arg("fabric.mod.json")
        .output()?;
    if !package.status.success() {
        return Err(format!(
            "jar failed to package the tiny test mod: {}",
            String::from_utf8_lossy(&package.stderr)
        )
        .into());
    }

    let config = scratch.0.join("config.json");
    let config_json = json!({
        "schema_version": 1,
        "java_home": java_home.clone(),
        "game_version": "26.3",
        "fabric_loader_jar": loader.clone(),
        "minecraft_client_jar": game.clone(),
        "libraries": [asm.clone(), asm_analysis.clone(), asm_commons.clone(), asm_tree.clone(), asm_util.clone(), mixin.clone()],
        "mods": [mod_jar],
        "callback_mod_id": "probe_mod",
        "callback_class": "dev.rustedition.probe.TestMod"
    });
    fs::write(&config, serde_json::to_vec_pretty(&config_json)?)?;

    let bridge_scratch = scratch.0.join("bridge");
    fs::create_dir_all(&bridge_scratch)?;
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_rust-edition-fabric-host"));
    let mut bridge = Bridge::spawn(SpawnConfig::new(&helper, &config, &bridge_scratch))?;
    let startup_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match bridge.status() {
            BridgeStatus::Ready => break,
            BridgeStatus::Failed(error) => {
                return Err(format!("Fabric host failed to start: {error}").into());
            }
            BridgeStatus::Stopped => {
                return Err("Fabric host stopped before it became ready".into());
            }
            BridgeStatus::Starting if Instant::now() < startup_deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            BridgeStatus::Starting => {
                return Err("Fabric host did not become ready in 60 seconds".into());
            }
        }
    }

    let snapshot = Snapshot {
        world_open: true,
        player: Some(PlayerSnapshot {
            x: 17.5,
            y: 4.0,
            z: 20.25,
            yaw: 90.0,
            pitch: -10.5,
            health: 19.0,
            dimension: "minecraft:overworld".into(),
        }),
        clock: Some(ClockSnapshot {
            game_time: 12_345,
            day_time: 6_789,
        }),
    };
    let sequence = bridge.try_send(snapshot.clone())?;
    let expected = java_string_hash(&snapshot_json(sequence, &snapshot)?).wrapping_add(2);
    let reply_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(reply) = bridge.try_recv() {
            if reply.seq != sequence {
                return Err(format!(
                    "received reply for sequence {}, expected {sequence}",
                    reply.seq
                )
                .into());
            }
            if reply.status != ResultStatus::Ok
                || reply.value.as_deref() != Some(expected.to_string().as_str())
            {
                return Err(format!(
                    "unexpected Fabric host reply: {reply:?}; expected {expected}"
                )
                .into());
            }
            break;
        }
        if let Some(error) = bridge.failure() {
            return Err(format!("Fabric host failed while handling a tick: {error}").into());
        }
        if Instant::now() >= reply_deadline {
            return Err("Fabric host did not reply to the snapshot within two seconds".into());
        }
        thread::sleep(Duration::from_millis(5));
    }

    bridge.shutdown();

    let failure_source = scratch.0.join("CauseFailureMod.java");
    let cycle_source = scratch.0.join("CycleFailureMod.java");
    let failure_classes = scratch.0.join("failure-classes");
    fs::create_dir_all(&failure_classes)?;
    fs::write(&failure_source, CAUSE_FAILURE_MOD)?;
    fs::write(&cycle_source, CYCLE_FAILURE_MOD)?;
    let compile_failures = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(&loader)
        .arg("-d")
        .arg(&failure_classes)
        .arg(&failure_source)
        .arg(&cycle_source)
        .output()?;
    if !compile_failures.status.success() {
        return Err(format!(
            "javac failed to build throwable diagnostic fixtures: {}",
            String::from_utf8_lossy(&compile_failures.stderr)
        )
        .into());
    }

    let failure_mod_jars = scratch.0.join("failure-mods");
    let cause_metadata_dir = scratch.0.join("cause-metadata");
    let cycle_metadata_dir = scratch.0.join("cycle-metadata");
    fs::create_dir_all(&failure_mod_jars)?;
    fs::create_dir_all(&cause_metadata_dir)?;
    fs::create_dir_all(&cycle_metadata_dir)?;
    fs::write(
        cause_metadata_dir.join("fabric.mod.json"),
        r#"{"schemaVersion":1,"id":"cause_failure_mod","version":"1.0.0","entrypoints":{"main":["dev.rustedition.probe.CauseFailureMod"]},"depends":{"fabricloader":">=0.19.5","minecraft":"26.3"}}"#,
    )?;
    fs::write(
        cycle_metadata_dir.join("fabric.mod.json"),
        r#"{"schemaVersion":1,"id":"cycle_failure_mod","version":"1.0.0","entrypoints":{"main":["dev.rustedition.probe.CycleFailureMod"]},"depends":{"fabricloader":">=0.19.5","minecraft":"26.3"}}"#,
    )?;
    let cause_mod_jar = failure_mod_jars.join("cause-failure-mod.jar");
    let cycle_mod_jar = failure_mod_jars.join("cycle-failure-mod.jar");
    package_mod(&jar, &cause_mod_jar, &failure_classes, &cause_metadata_dir)?;
    package_mod(&jar, &cycle_mod_jar, &failure_classes, &cycle_metadata_dir)?;

    for (mod_id, callback_class, mod_jar, expected, label) in [
        (
            "cause_failure_mod",
            "dev.rustedition.probe.CauseFailureMod",
            cause_mod_jar,
            "root cause: java.lang.IllegalArgumentException: fixture root cause",
            "wrapped Java cause",
        ),
        (
            "cycle_failure_mod",
            "dev.rustedition.probe.CycleFailureMod",
            cycle_mod_jar,
            "cause cycle stopped",
            "Java cause cycle",
        ),
    ] {
        let failure_config_path = scratch.0.join(format!("{mod_id}-config.json"));
        let failure_config_json = json!({
            "schema_version": 1,
            "java_home": java_home.clone(),
            "game_version": "26.3",
            "fabric_loader_jar": loader.clone(),
            "minecraft_client_jar": game.clone(),
            "libraries": [asm.clone(), asm_analysis.clone(), asm_commons.clone(), asm_tree.clone(), asm_util.clone(), mixin.clone()],
            "mods": [mod_jar],
            "callback_mod_id": mod_id,
            "callback_class": callback_class
        });
        fs::write(
            &failure_config_path,
            serde_json::to_vec_pretty(&failure_config_json)?,
        )?;
        let failure = expect_startup_failure(
            &helper,
            &failure_config_path,
            &scratch.0.join(format!("{mod_id}-bridge")),
            expected,
        )?;
        eprintln!("Fabric host reported a bounded {label}: {failure}");
    }

    eprintln!(
        "Fabric host checks passed: snapshot callback, wrapped Java cause, and cause-cycle handling."
    );
    Ok(())
}

fn java_string_hash(value: &str) -> i32 {
    value.encode_utf16().fold(0_i32, |hash, character| {
        hash.wrapping_mul(31).wrapping_add(i32::from(character))
    })
}
