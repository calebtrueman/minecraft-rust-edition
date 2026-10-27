use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rust_edition_fabric::inspect_jar;
use rust_edition_mod_bridge::{
    Bridge, BridgeError, BridgeStatus, ClockSnapshot, FramebufferSnapshot, MAX_VALUE_BYTES,
    PlayerSnapshot, ResultStatus, Snapshot, SpawnConfig, StateReason, snapshot_json, state_json,
};
use serde_json::json;

const OPT_IN: &str = "RUST_EDITION_FABRIC_HOST_TEST";
const SKIP_FAILURE_PROBES: &str = "RUST_EDITION_FABRIC_HOST_TEST_SKIP_FAILURE_PROBES";
const TEST_MOD: &str = r#"
package dev.rustedition.probe;

import net.fabricmc.api.ClientModInitializer;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents;
import net.minecraft.client.Minecraft;
import net.minecraft.IdentifierException;
import net.minecraft.resources.Identifier;

public final class TestMod implements ClientModInitializer, SnapshotTick {
    public static native int hostCall();
    public static native void observeWindow(
        Minecraft client, com.mojang.blaze3d.platform.Window window, int width, int height);
    private static int endTickCalls;
    private static int lateListenerCalls;
    private static boolean lateListenerRegistered;

    @Override
    public void onInitializeClient() {
        System.setProperty("probe.initialized", Integer.toString(hostCall()));
        ClientTickEvents.END_CLIENT_TICK.register(client -> {
            endTickCalls++;
            com.mojang.blaze3d.platform.Window window = client.getWindow();
            observeWindow(client, window, window.getWidth(), window.getHeight());
        });
    }

    @Override
    public String onSnapshot(String snapshotJson) {
        if (!lateListenerRegistered
                && lateListenerCalls == 0
                && snapshotJson.contains("\"kind\":\"tick\"")) {
            lateListenerRegistered = true;
            ClientTickEvents.END_CLIENT_TICK.register(client -> lateListenerCalls++);
        }
        return Integer.toString(
                snapshotJson.hashCode() + hostCall() + endTickCalls + lateListenerCalls)
                + "|" + identifierResult() + "|" + codecPolicyResult();
    }

    private static String identifierResult() {
        Identifier base = Identifier.fromNamespaceAndPath("demo", "blocks/stone");
        Identifier defaultId = Identifier.withDefaultNamespace("blocks/stone");
        String invalid;
        try {
            Identifier.parse("Demo:path");
            invalid = "missing";
        } catch (IdentifierException error) {
            invalid = error.getClass().getName() + ":" + error.getMessage();
        }
        String actual = String.join("|",
                base.toString(),
                base.getNamespace(),
                base.getPath(),
                base.withPrefix("prefix/").toString(),
                base.withSuffix(".json").toString(),
                base.toLanguageKey("block", "stone"),
                base.toDebugFileName(),
                defaultId.toShortLanguageKey(),
                defaultId.toShortString(),
                String.valueOf(Identifier.tryParse("Demo:path")),
                Boolean.toString(Identifier.isValidNamespace("a..b")),
                Boolean.toString(Identifier.isValidNamespace("..")),
                Identifier.bySeparator("demo·blocks/stone", '·').toString(),
                Integer.toString(base.hashCode()),
                Integer.toString(Identifier.fromNamespaceAndPath("demo", "z")
                        .compareTo(Identifier.fromNamespaceAndPath("demo", "a"))),
                Boolean.toString(base.equals(Identifier.parse("demo:blocks/stone"))),
                Boolean.toString(base.equals(null)),
                Boolean.toString(base.equals("demo:blocks/stone")),
                invalid);
        String expected = "__IDENTIFIER_REFERENCE_RESULT__";
        if (!actual.equals(expected)) {
            int difference = 0;
            while (difference < actual.length()
                    && difference < expected.length()
                    && actual.charAt(difference) == expected.charAt(difference)) {
                difference++;
            }
            String expectedUnit = difference < expected.length()
                    ? Integer.toHexString(expected.charAt(difference)) : "end";
            String actualUnit = difference < actual.length()
                    ? Integer.toHexString(actual.charAt(difference)) : "end";
            throw new AssertionError("Identifier result mismatch at char " + difference
                    + " (expected U+" + expectedUnit + ", actual U+" + actualUnit
                    + "; lengths " + expected.length() + "/" + actual.length() + ")");
        }
        String factoryOrder = factoryOrderResult();
        String expectedFactoryOrder = "__FACTORY_ORDER_REFERENCE_RESULT__";
        if (!factoryOrder.equals(expectedFactoryOrder)) {
            throw new AssertionError("Factory null order mismatch: " + factoryOrder);
        }
        return "identifiers-ok";
    }

    private static String factoryOrderResult() {
        String invalidNamespaceAndNullPath =
                String.valueOf(Identifier.tryBuild("Demo", null));
        String bothNull;
        try {
            Identifier.tryBuild(null, null);
            bothNull = "missing";
        } catch (NullPointerException error) {
            bothNull = error.getClass().getName() + ":" + error.getMessage();
        }
        return invalidNamespaceAndNullPath + "|" + bothNull;
    }

    private static String codecPolicyResult() {
        try {
            Object codec = Identifier.CODEC;
            return codec == null ? "null" : "read";
        } catch (UnsupportedOperationException error) {
            return "blocked";
        }
    }
}
"#;

const IDENTIFIER_REFERENCE_RESULT: &str = "demo:blocks/stone|demo|blocks/stone|demo:prefix/blocks/stone|demo:blocks/stone.json|block.demo.blocks/stone.stone|demo_blocks_stone|blocks/stone|blocks/stone|null|true|false|demo:blocks/stone|-1882878695|25|true|false|false|net.minecraft.IdentifierException:Non [a-z0-9_.-] character in namespace of identifier: Demo:path";
const FACTORY_ORDER_REFERENCE_RESULT: &str = r#"null|java.lang.NullPointerException:Cannot invoke "String.equals(Object)" because "namespace" is null"#;

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

fn minecraft_runtime_libraries(
    minecraft_home: &Path,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let manifest_path = minecraft_home.join("versions/26.3/26.3.json");
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let libraries = manifest
        .get("libraries")
        .and_then(serde_json::Value::as_array)
        .ok_or("Minecraft 26.3 manifest has no libraries array")?;
    let mut checked = Vec::new();
    for library in libraries {
        let coordinate = library
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Minecraft library entry has no name")?;
        // A four-part Maven coordinate names a native classifier, not a Java class-path JAR.
        if coordinate.split(':').count() > 3 {
            continue;
        }
        let Some(artifact) = library.get("downloads").and_then(|v| v.get("artifact")) else {
            continue;
        };
        let Some(relative_path) = artifact.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if relative_path.contains("-natives-") {
            continue;
        }
        let relative = Path::new(relative_path);
        if relative.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(
                format!("unsafe path in Minecraft library manifest: {relative_path}").into(),
            );
        }
        let path = minecraft_home.join("libraries").join(relative);
        let metadata = fs::metadata(&path).map_err(|error| {
            format!("missing Minecraft 26.3 Java library {relative_path}: {error}")
        })?;
        if let Some(expected_size) = artifact.get("size").and_then(serde_json::Value::as_u64)
            && metadata.len() != expected_size
        {
            return Err(format!(
                "Minecraft library {relative_path} has {} bytes, expected {expected_size}",
                metadata.len()
            )
            .into());
        }
        if let Some(expected_sha1) = artifact.get("sha1").and_then(serde_json::Value::as_str) {
            let mut file = File::open(&path)?;
            let mut hash = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            let actual_sha1 = hash
                .finish()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if actual_sha1 != expected_sha1 {
                return Err(format!(
                    "Minecraft library {relative_path} has SHA-1 {actual_sha1}, expected {expected_sha1}"
                )
                .into());
            }
        }
        checked.push(fs::canonicalize(path)?);
    }
    if checked.is_empty() {
        return Err("Minecraft 26.3 manifest had no ordinary Java libraries".into());
    }
    Ok(checked)
}

fn java_tool(java_home: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    let name = format!("{name}.exe");
    #[cfg(not(windows))]
    let name = name.to_owned();
    java_home.join("bin").join(name)
}

fn extract_fabric_event_jars(
    fabric_api_jar: &Path,
    scratch: &Path,
) -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let report = inspect_jar(fabric_api_jar)?;
    let fabric_api = report
        .mods
        .iter()
        .find(|module| module.id == "fabric-api")
        .ok_or("configured Fabric API JAR does not declare fabric-api")?;
    if fabric_api.version != "0.162.0+26.3" {
        return Err(format!(
            "Fabric API JAR must be 0.162.0+26.3, found {}",
            fabric_api.version
        )
        .into());
    }
    let nested_path = |prefix: &str| -> Result<String, Box<dyn std::error::Error>> {
        let matches = fabric_api
            .nested_jars
            .iter()
            .filter(|path| {
                path.rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(format!(
                "Fabric API must declare exactly one {prefix} nested JAR, found {}",
                matches.len()
            )
            .into());
        }
        Ok(matches[0].clone())
    };
    let base_entry = nested_path("fabric-api-base-")?;
    let lifecycle_entry = nested_path("fabric-lifecycle-events-v1-")?;
    let base = scratch.join("fabric-api-base.jar");
    let lifecycle = scratch.join("fabric-lifecycle-events-v1.jar");
    for (entry_name, destination) in [
        (base_entry, base.as_path()),
        (lifecycle_entry, lifecycle.as_path()),
    ] {
        let mut archive = zip::ZipArchive::new(File::open(fabric_api_jar)?)?;
        let entry = archive.by_name(&entry_name)?;
        if entry.is_dir() {
            return Err(format!("Fabric API nested JAR is a directory: {entry_name}").into());
        }
        let mut output = File::create(destination)?;
        let size = entry.size();
        let limit = rust_edition_fabric::Limits::default().max_nested_jar_bytes;
        if size > limit {
            return Err(
                format!("Fabric API nested JAR exceeds {limit} bytes: {entry_name}").into(),
            );
        }
        let mut limited_entry = entry.take(limit + 1);
        let copied = io::copy(&mut limited_entry, &mut output)?;
        if copied > limit || copied != size {
            return Err(
                format!("Fabric API nested JAR was only partly copied: {entry_name}").into(),
            );
        }
    }
    Ok((base, lifecycle))
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
    let deadline = Instant::now() + Duration::from_secs(120);
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
                    "the failing Fabric helper did not report an error in 120 seconds".into(),
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
    let minecraft_home = required_path("RUST_EDITION_JAVA_FABRIC_MINECRAFT_HOME", true)?;
    let java_home = required_path("RUST_EDITION_JAVA25_HOME", true)?;
    let loader = required_path("RUST_EDITION_JAVA_FABRIC_LOADER_JAR", false)?;
    let game = required_path("RUST_EDITION_JAVA_FABRIC_GAME_JAR", false)?;
    let fabric_api = required_path("RUST_EDITION_JAVA_FABRIC_API_JAR", false)?;
    let asm = required_path("RUST_EDITION_JAVA_FABRIC_ASM_JAR", false)?;
    let asm_analysis = required_path("RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR", false)?;
    let asm_commons = required_path("RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR", false)?;
    let asm_tree = required_path("RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR", false)?;
    let asm_util = required_path("RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR", false)?;
    let mixin = required_path("RUST_EDITION_JAVA_FABRIC_MIXIN_JAR", false)?;
    let guava = required_path("RUST_EDITION_JAVA_FABRIC_GUAVA_JAR", false)?;
    let failureaccess = required_path("RUST_EDITION_JAVA_FABRIC_FAILUREACCESS_JAR", false)?;
    let mut libraries = minecraft_runtime_libraries(&minecraft_home)?;
    let mut seen_libraries = libraries.iter().cloned().collect::<HashSet<_>>();
    for library in [
        &asm,
        &asm_analysis,
        &asm_commons,
        &asm_tree,
        &asm_util,
        &mixin,
        &guava,
        &failureaccess,
    ] {
        if seen_libraries.insert(library.clone()) {
            libraries.push(library.clone());
        }
    }
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
    let (fabric_api_base, fabric_lifecycle) = extract_fabric_event_jars(&fabric_api, &scratch.0)?;
    let mod_source = scratch.0.join("TestMod.java");
    let tick_source = scratch.0.join("SnapshotTick.java");
    let escaped_identifier_reference = escape_java_string_literal(IDENTIFIER_REFERENCE_RESULT);
    let escaped_factory_reference = escape_java_string_literal(FACTORY_ORDER_REFERENCE_RESULT);
    let test_mod_source = TEST_MOD
        .replace(
            "__IDENTIFIER_REFERENCE_RESULT__",
            &escaped_identifier_reference,
        )
        .replace(
            "__FACTORY_ORDER_REFERENCE_RESULT__",
            &escaped_factory_reference,
        );
    fs::write(&mod_source, test_mod_source)?;
    fs::write(&tick_source, SNAPSHOT_TICK)?;
    let mut compile_class_path_entries = vec![
        loader.clone(),
        game.clone(),
        fabric_api_base,
        fabric_lifecycle,
    ];
    compile_class_path_entries.extend(libraries.iter().cloned());
    let compile_class_path = env::join_paths(compile_class_path_entries)?;
    let compile = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(compile_class_path)
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
        r#"{"schemaVersion":1,"id":"probe_mod","version":"1.0.0","entrypoints":{"client":["dev.rustedition.probe.TestMod"]},"depends":{"fabricloader":">=0.19.5","minecraft":"26.3"}}"#,
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
        "schema_version": 2,
        "java_home": java_home.clone(),
        "game_version": "26.3",
        "fabric_loader_jar": loader.clone(),
        "fabric_api_jar": fabric_api.clone(),
        "minecraft_client_jar": game.clone(),
        "libraries": libraries,
        "mods": [mod_jar],
        "callback_mod_id": "probe_mod",
        "callback_class": "dev.rustedition.probe.TestMod"
    });
    fs::write(&config, serde_json::to_vec_pretty(&config_json)?)?;

    let bridge_scratch = scratch.0.join("bridge");
    fs::create_dir_all(&bridge_scratch)?;
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_rust-edition-fabric-host"));
    let mut bridge = Bridge::spawn(SpawnConfig::new(&helper, &config, &bridge_scratch))?;
    let startup_deadline = Instant::now() + Duration::from_secs(120);
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
                return Err("Fabric host did not become ready in 120 seconds".into());
            }
        }
    }

    let initial_snapshot = Snapshot {
        framebuffer: FramebufferSnapshot {
            width: 640,
            height: 360,
        },
        ..Snapshot::default()
    };
    let initial_sequence = bridge.try_send_state(StateReason::Initial, initial_snapshot.clone())?;
    let initial_expected = format!(
        "{}|identifiers-ok|blocked",
        java_string_hash(&state_json(
            initial_sequence,
            StateReason::Initial,
            &initial_snapshot,
        )?)
        .wrapping_add(2)
    );
    wait_for_result(
        &bridge,
        initial_sequence,
        &initial_expected,
        "initial state",
    )
    .await?;

    let title_snapshot = Snapshot {
        framebuffer: FramebufferSnapshot {
            width: 960,
            height: 540,
        },
        ..Snapshot::default()
    };
    let title_sequence = bridge.try_send_tick(title_snapshot.clone())?;
    let title_expected = format!(
        "{}|identifiers-ok|blocked",
        java_string_hash(&snapshot_json(title_sequence, &title_snapshot)?).wrapping_add(4)
    );
    wait_for_result(&bridge, title_sequence, &title_expected, "title tick").await?;

    let snapshot = Snapshot {
        framebuffer: FramebufferSnapshot {
            width: 1280,
            height: 720,
        },
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
    let sequence = bridge.try_send_tick(snapshot.clone())?;
    let expected = format!(
        "{}|identifiers-ok|blocked",
        java_string_hash(&snapshot_json(sequence, &snapshot)?).wrapping_add(7)
    );
    wait_for_result(&bridge, sequence, &expected, "world tick").await?;

    let close_snapshot = Snapshot {
        framebuffer: snapshot.framebuffer,
        ..Snapshot::default()
    };
    let close_sequence = bridge.try_send_state(StateReason::WorldClosed, close_snapshot.clone())?;
    let close_expected = format!(
        "{}|identifiers-ok|blocked",
        java_string_hash(&state_json(
            close_sequence,
            StateReason::WorldClosed,
            &close_snapshot,
        )?)
        .wrapping_add(8)
    );
    wait_for_result(
        &bridge,
        close_sequence,
        &close_expected,
        "world close state",
    )
    .await?;

    bridge.shutdown();

    if matches!(env::var(SKIP_FAILURE_PROBES).as_deref(), Ok("1")) {
        eprintln!("skipping the two startup-failure probes for this focused callback check");
        return Ok(());
    }

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
            "schema_version": 2,
            "java_home": java_home.clone(),
            "game_version": "26.3",
            "fabric_loader_jar": loader.clone(),
            "fabric_api_jar": fabric_api.clone(),
            "minecraft_client_jar": game.clone(),
            "libraries": libraries.clone(),
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
        "Fabric host checks passed: real END_CLIENT_TICK dispatch on title and world frames, stable Minecraft/Window identity, snapshot callback, wrapped Java cause, and cause-cycle handling."
    );
    Ok(())
}

fn java_string_hash(value: &str) -> i32 {
    value.encode_utf16().fold(0_i32, |hash, character| {
        hash.wrapping_mul(31).wrapping_add(i32::from(character))
    })
}

fn escape_java_string_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn wait_for_result(
    bridge: &Bridge,
    sequence: u64,
    expected: &str,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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
            if reply.status != ResultStatus::Ok || reply.value.as_deref() != Some(expected) {
                return Err(format!(
                    "unexpected Fabric host {label} reply: {reply:?}; expected {expected:?}"
                )
                .into());
            }
            return Ok(());
        }
        if let Some(error) = bridge.failure() {
            return Err(format!("Fabric host failed while handling {label}: {error}").into());
        }
        if Instant::now() >= reply_deadline {
            return Err(format!("Fabric host did not reply to {label} within two seconds").into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}
