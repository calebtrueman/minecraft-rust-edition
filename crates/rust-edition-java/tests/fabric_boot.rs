use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ristretto_vm::{Error as VmError, Parameters};
use rust_edition_java::{JavaVm, NativeMethod, NativeResult, RustValue, Thread, Value};
use serde_json::Value as JsonValue;
use zip::ZipArchive;

const OPT_IN: &str = "RUST_EDITION_JAVA_FABRIC_BOOT";
const LOADER_VERSION: &str = "0.19.5";
const GAME_VERSION: &str = "26.3";

const LOADER_JAR: &str = "RUST_EDITION_JAVA_FABRIC_LOADER_JAR";
const GAME_JAR: &str = "RUST_EDITION_JAVA_FABRIC_GAME_JAR";
const ASM_JAR: &str = "RUST_EDITION_JAVA_FABRIC_ASM_JAR";
const ASM_ANALYSIS_JAR: &str = "RUST_EDITION_JAVA_FABRIC_ASM_ANALYSIS_JAR";
const ASM_COMMONS_JAR: &str = "RUST_EDITION_JAVA_FABRIC_ASM_COMMONS_JAR";
const ASM_TREE_JAR: &str = "RUST_EDITION_JAVA_FABRIC_ASM_TREE_JAR";
const ASM_UTIL_JAR: &str = "RUST_EDITION_JAVA_FABRIC_ASM_UTIL_JAR";
const MIXIN_JAR: &str = "RUST_EDITION_JAVA_FABRIC_MIXIN_JAR";

const TEST_MOD: &str = r#"
package dev.rustedition.probe;

import net.fabricmc.api.ModInitializer;

public final class TestMod implements ModInitializer, SnapshotTick {
    public static native int hostCall();

    @Override
    public void onInitialize() {
        int calls = hostCall();
        System.setProperty("probe.host_callback_result", Integer.toString(calls));
        System.setProperty("probe.test_mod_initialized", "yes");
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

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rust edition java fabric boot-雪-#-%-{}-{unique}",
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

fn required_directory(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let value = env::var_os(name).ok_or_else(|| format!("set {name} to a directory"))?;
    let path = PathBuf::from(value);
    if !path.is_dir() {
        return Err(format!("{name} must point to a directory: {}", path.display()).into());
    }
    Ok(path)
}

fn required_file(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let value = env::var_os(name).ok_or_else(|| format!("set {name} to a JAR file"))?;
    let path = PathBuf::from(value);
    if !path.is_file() {
        return Err(format!("{name} must point to a file: {}", path.display()).into());
    }
    Ok(path)
}

fn java_tool(java_home: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    let executable = format!("{name}.exe");
    #[cfg(not(windows))]
    let executable = name.to_string();
    java_home.join("bin").join(executable)
}

fn check_java_25_compiler(javac: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !javac.is_file() {
        return Err(format!("JAVA_HOME must contain {}", javac.display()).into());
    }
    let output = Command::new(javac).arg("-version").output()?;
    let version = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || !version.contains("javac 25") {
        return Err(format!("JAVA_HOME must provide a Java 25 compiler; got {version}").into());
    }
    Ok(())
}

fn validate_library_home(home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let release = fs::read_to_string(home.join("release"))?;
    let version = release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .map(|version| version.trim_matches('"'))
        .ok_or("RUST_EDITION_JAVA25_HOME/release must declare JAVA_VERSION")?;
    if version.split(['.', '+', '-']).next() != Some("25") {
        return Err(format!("RUST_EDITION_JAVA25_HOME must be Java 25; found {version}").into());
    }
    if home.join("lib/modules").exists() {
        return Err(
            "RUST_EDITION_JAVA25_HOME must use JMODs without lib/modules; the image path is unsupported here"
                .into(),
        );
    }

    for module in [
        "java.base",
        "java.compiler",
        "java.datatransfer",
        "java.desktop",
        "java.logging",
        "java.sql",
        "java.transaction.xa",
        "java.xml",
    ] {
        let path = home.join("jmods").join(format!("{module}.jmod"));
        if !path.is_file() {
            return Err(format!(
                "RUST_EDITION_JAVA25_HOME is missing required module {module}: {}",
                path.display()
            )
            .into());
        }
    }
    for relative in ["lib/tzdb.dat", "conf/security/java.security"] {
        let path = home.join(relative);
        if !path.is_file() {
            return Err(format!(
                "RUST_EDITION_JAVA25_HOME is missing required file {}",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn copy_jar(source: &Path, classpath: &Path, file_name: &str) -> std::io::Result<PathBuf> {
    let copy = classpath.join(file_name);
    fs::copy(source, &copy)?;
    Ok(copy)
}

fn read_json_string(
    archive_path: &Path,
    entry_name: &str,
    field: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut entry = archive.by_name(entry_name)?;
    if entry.size() > 1_048_576 {
        return Err(
            format!("{archive_path:?}:{entry_name} metadata is too large to inspect").into(),
        );
    }
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    let document: JsonValue = serde_json::from_str(&text)?;
    document
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            format!("{archive_path:?}:{entry_name} has no string field {field:?}").into()
        })
}

#[allow(
    clippy::result_large_err,
    reason = "the callback signature must use Ristretto's native-method error type"
)]
fn host_call(thread: Arc<Thread>, _parameters: Parameters) -> NativeResult<Option<Value>> {
    let state = thread
        .vm()?
        .host_context()
        .ok_or_else(|| VmError::InternalError("Fabric callback has no host state".into()))?;
    let counter = Arc::downcast::<AtomicUsize>(state).map_err(|_| {
        VmError::InternalError("Fabric callback host state has the wrong type".into())
    })?;
    let result = counter.fetch_add(1, Ordering::SeqCst) + 1;
    let result = i32::try_from(result)
        .map_err(|error| VmError::InternalError(format!("callback count is too large: {error}")))?;
    Ok(Some(Value::Int(result)))
}

async fn invoke_value(
    vm: &JavaVm,
    class_name: &str,
    method_signature: &str,
    arguments: &[impl RustValue],
) -> Result<Value, Box<dyn std::error::Error>> {
    vm.invoke(class_name, method_signature, arguments)
        .await?
        .ok_or_else(|| format!("{class_name}.{method_signature} returned no value").into())
}

async fn set_property(
    vm: &JavaVm,
    key: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = vm
        .invoke(
            "java.lang.System",
            "setProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            &[key, value],
        )
        .await?;
    Ok(())
}

async fn class_object(vm: &JavaVm, class_name: &str) -> Result<Value, Box<dyn std::error::Error>> {
    invoke_value(
        vm,
        "java.lang.Class",
        "forName(Ljava/lang/String;)Ljava/lang/Class;",
        &[class_name],
    )
    .await
}

async fn java_string(vm: &JavaVm, value: &str) -> Result<Value, Box<dyn std::error::Error>> {
    vm.new_object("java.lang.String", "Ljava/lang/String;", &[value])
        .await
        .map_err(Into::into)
}

async fn string_array(vm: &JavaVm, values: &[&str]) -> Result<Value, Box<dyn std::error::Error>> {
    let string_class = class_object(vm, "java.lang.String").await?;
    let array = invoke_value(
        vm,
        "java.lang.reflect.Array",
        "newInstance(Ljava/lang/Class;I)Ljava/lang/Object;",
        &[string_class, Value::Int(i32::try_from(values.len())?)],
    )
    .await?;

    for (index, value) in values.iter().enumerate() {
        let value = java_string(vm, value).await?;
        let arguments = [array.clone(), Value::Int(i32::try_from(index)?), value];
        let _ = vm
            .invoke(
                "java.lang.reflect.Array",
                "set(Ljava/lang/Object;ILjava/lang/Object;)V",
                &arguments,
            )
            .await?;
    }
    Ok(array)
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_bootstrap_starts_fabric_and_calls_a_mod_snapshot_method()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(opt_in) = env::var_os(OPT_IN) else {
        eprintln!(
            "skipping: set {OPT_IN}=1 and provide all Java 25, Fabric, Mixin, ASM, and Minecraft JAR paths"
        );
        return Ok(());
    };
    if opt_in != "1" {
        return Err(format!("{OPT_IN} must be exactly 1 when set").into());
    }

    let jdk_home = required_directory("JAVA_HOME")?;
    let javac = java_tool(&jdk_home, "javac");
    let jar_tool = java_tool(&jdk_home, "jar");
    check_java_25_compiler(&javac)?;
    if !jar_tool.is_file() {
        return Err(format!("JAVA_HOME must contain {}", jar_tool.display()).into());
    }

    let library_home = required_directory("RUST_EDITION_JAVA25_HOME")?;
    validate_library_home(&library_home)?;

    let loader_source = required_file(LOADER_JAR)?;
    let game_source = required_file(GAME_JAR)?;
    let asm_source = required_file(ASM_JAR)?;
    let asm_analysis_source = required_file(ASM_ANALYSIS_JAR)?;
    let asm_commons_source = required_file(ASM_COMMONS_JAR)?;
    let asm_tree_source = required_file(ASM_TREE_JAR)?;
    let asm_util_source = required_file(ASM_UTIL_JAR)?;
    let mixin_source = required_file(MIXIN_JAR)?;

    let scratch = ScratchDir::new()?;
    let game_dir = scratch.0.join("game");
    let mods_dir = game_dir.join("mods");
    let java_temp_dir = game_dir.join("tmp");
    let classpath_dir = scratch.0.join("classpath");
    let mod_classes = scratch.0.join("mod-classes");
    fs::create_dir_all(&mods_dir)?;
    fs::create_dir_all(&java_temp_dir)?;
    fs::create_dir_all(&classpath_dir)?;
    fs::create_dir_all(&mod_classes)?;

    let loader_jar = copy_jar(&loader_source, &classpath_dir, "fabric-loader.jar")?;
    let game_jar = copy_jar(&game_source, &classpath_dir, "minecraft-client.jar")?;
    let asm_jar = copy_jar(&asm_source, &classpath_dir, "asm.jar")?;
    let asm_analysis_jar = copy_jar(&asm_analysis_source, &classpath_dir, "asm-analysis.jar")?;
    let asm_commons_jar = copy_jar(&asm_commons_source, &classpath_dir, "asm-commons.jar")?;
    let asm_tree_jar = copy_jar(&asm_tree_source, &classpath_dir, "asm-tree.jar")?;
    let asm_util_jar = copy_jar(&asm_util_source, &classpath_dir, "asm-util.jar")?;
    let mixin_jar = copy_jar(&mixin_source, &classpath_dir, "sponge-mixin.jar")?;

    let loader_id = read_json_string(&loader_jar, "fabric.mod.json", "id")?;
    let loader_version = read_json_string(&loader_jar, "fabric.mod.json", "version")?;
    if loader_id != "fabricloader" || loader_version != LOADER_VERSION {
        return Err(format!(
            "{LOADER_JAR} must be Fabric Loader {LOADER_VERSION}; found {loader_id} {loader_version}"
        )
        .into());
    }
    let game_version = read_json_string(&game_jar, "version.json", "id")?;
    if game_version != GAME_VERSION {
        return Err(
            format!("{GAME_JAR} must be Minecraft {GAME_VERSION}; found {game_version}").into(),
        );
    }

    let mod_source = scratch.0.join("TestMod.java");
    let tick_source = scratch.0.join("SnapshotTick.java");
    fs::write(&mod_source, TEST_MOD)?;
    fs::write(&tick_source, SNAPSHOT_TICK)?;
    let compile_mod = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(&loader_jar)
        .arg("-d")
        .arg(&mod_classes)
        .arg(&mod_source)
        .arg(&tick_source)
        .output()?;
    if !compile_mod.status.success() {
        return Err(format!(
            "javac failed to build the test mod: {}",
            String::from_utf8_lossy(&compile_mod.stderr)
        )
        .into());
    }

    let metadata_path = scratch.0.join("fabric.mod.json");
    fs::write(
        &metadata_path,
        format!(
            r#"{{"schemaVersion":1,"id":"probe_mod","version":"1.0.0","entrypoints":{{"main":["dev.rustedition.probe.TestMod"]}},"depends":{{"fabricloader":">={LOADER_VERSION}","minecraft":"{GAME_VERSION}"}}}}"#
        ),
    )?;
    let mod_jar = mods_dir.join("probe-mod.jar");
    let package_mod = Command::new(&jar_tool)
        .arg("--create")
        .arg("--file")
        .arg(&mod_jar)
        .arg("-C")
        .arg(&mod_classes)
        .arg(".")
        .arg("-C")
        .arg(&scratch.0)
        .arg("fabric.mod.json")
        .output()?;
    if !package_mod.status.success() {
        return Err(format!(
            "jar failed to package the test mod: {}",
            String::from_utf8_lossy(&package_mod.stderr)
        )
        .into());
    }

    let class_paths = vec![
        loader_jar.clone(),
        game_jar.clone(),
        mod_jar.clone(),
        asm_jar,
        asm_analysis_jar,
        asm_commons_jar,
        asm_tree_jar,
        asm_util_jar,
        mixin_jar,
    ];

    let class_path = env::join_paths(&class_paths)?
        .to_string_lossy()
        .into_owned();
    let game_dir_path = game_dir.to_string_lossy().into_owned();
    let game_jar_path = game_jar.to_string_lossy().into_owned();
    let mod_jar_path = mod_jar.to_string_lossy().into_owned();
    let java_temp_dir_path = java_temp_dir.to_string_lossy().into_owned();
    let mods_dir_path = mods_dir.to_string_lossy().into_owned();
    let fabric_log_path = game_dir.join("fabric.log").to_string_lossy().into_owned();
    let callback_count = Arc::new(AtomicUsize::new(0));
    let vm = JavaVm::new_with_host_context(
        &library_home,
        &class_paths,
        [NativeMethod::sync(
            "dev/rustedition/probe/TestMod.hostCall()I",
            host_call,
        )],
        Arc::clone(&callback_count),
    )
    .await?;
    if !vm.java_version().starts_with("25.") {
        return Err(format!(
            "the prepared library home resolved to unexpected Java version {}",
            vm.java_version()
        )
        .into());
    }
    for (key, value) in [
        ("java.class.path", class_path.as_str()),
        ("user.home", game_dir_path.as_str()),
        ("user.dir", game_dir_path.as_str()),
        ("java.io.tmpdir", java_temp_dir_path.as_str()),
        ("fabric.gameVersion", GAME_VERSION),
        ("fabric.gameJarPath", game_jar_path.as_str()),
        ("fabric.addMods", mod_jar_path.as_str()),
        ("fabric.modsFolder", mods_dir_path.as_str()),
        ("fabric.log.file", fabric_log_path.as_str()),
    ] {
        set_property(&vm, key, value).await?;
    }

    let env_type = invoke_value(
        &vm,
        "net.fabricmc.api.EnvType",
        "valueOf(Ljava/lang/String;)Lnet/fabricmc/api/EnvType;",
        &["CLIENT"],
    )
    .await?;
    let knot = vm
        .new_object(
            "net.fabricmc.loader.impl.launch.knot.Knot",
            "Lnet/fabricmc/api/EnvType;",
            &[env_type],
        )
        .await?;
    let arguments = string_array(
        &vm,
        &[
            "--gameDir",
            game_dir_path.as_str(),
            "--version",
            GAME_VERSION,
        ],
    )
    .await?;
    let arguments = [knot.clone(), arguments];
    let _ = vm
        .invoke(
            "net.fabricmc.loader.impl.launch.knot.Knot",
            "init([Ljava/lang/String;)Ljava/lang/ClassLoader;",
            &arguments,
        )
        .await?;

    let loader = invoke_value(
        &vm,
        "net.fabricmc.loader.api.FabricLoader",
        "getInstance()Lnet/fabricmc/loader/api/FabricLoader;",
        &[] as &[i32],
    )
    .await?;
    let event_name = java_string(&vm, "main").await?;
    let initializer_class = class_object(&vm, "net.fabricmc.api.ModInitializer").await?;
    let entrypoint_arguments = [loader, event_name, initializer_class];
    let entrypoints = invoke_value(
        &vm,
        "net.fabricmc.loader.api.FabricLoader",
        "getEntrypoints(Ljava/lang/String;Ljava/lang/Class;)Ljava/util/List;",
        &entrypoint_arguments,
    )
    .await?;
    let entrypoint_count = invoke_value(
        &vm,
        "java.util.List",
        "size()I",
        std::slice::from_ref(&entrypoints),
    )
    .await?
    .as_i32()?;
    if entrypoint_count <= 0 {
        return Err("Fabric did not find any main entrypoints".into());
    }

    let snapshot_tick_class = class_object(&vm, "dev.rustedition.probe.SnapshotTick").await?;
    let snapshot_json = r#"{"world_open":true,"player":{"x":17.5,"y":4.0,"z":20.25,"yaw":90.0,"pitch":-10.5,"health":19.0,"dimension":"minecraft:overworld"},"clock":{"game_time":12345,"day_time":6789}}"#;
    let expected_snapshot_result = java_string_hash(snapshot_json).wrapping_add(2);
    let mut snapshot_result = None;
    for index in 0..entrypoint_count {
        let arguments = [entrypoints.clone(), Value::Int(index)];
        let entrypoint = invoke_value(
            &vm,
            "java.util.List",
            "get(I)Ljava/lang/Object;",
            &arguments,
        )
        .await?;
        let _ = vm
            .invoke(
                "net.fabricmc.api.ModInitializer",
                "onInitialize()V",
                std::slice::from_ref(&entrypoint),
            )
            .await?;

        let is_snapshot_tick = invoke_value(
            &vm,
            "java.lang.Class",
            "isInstance(Ljava/lang/Object;)Z",
            &[snapshot_tick_class.clone(), entrypoint.clone()],
        )
        .await?
        .as_i32()?;
        if is_snapshot_tick != 0 {
            if snapshot_result.is_some() {
                return Err("more than one main entrypoint implements SnapshotTick".into());
            }
            let snapshot_json = java_string(&vm, snapshot_json).await?;
            let arguments = [entrypoint, snapshot_json];
            let result = invoke_value(
                &vm,
                "dev.rustedition.probe.SnapshotTick",
                "onSnapshot(Ljava/lang/String;)Ljava/lang/String;",
                &arguments,
            )
            .await?;
            snapshot_result = Some(result.as_string()?.to_owned());
        }
    }

    let marker = system_property(&vm, "probe.test_mod_initialized").await?;
    let callback_result = system_property(&vm, "probe.host_callback_result").await?;
    if marker != "yes" || callback_result != "1" {
        return Err(format!(
            "Fabric did not initialize the test mod: marker={marker:?}, callback result={callback_result:?}"
        )
        .into());
    }
    if snapshot_result.as_deref() != Some(expected_snapshot_result.to_string().as_str()) {
        return Err(format!(
            "Fabric entrypoint did not receive the full host snapshot; expected hash {expected_snapshot_result}, got {snapshot_result:?}"
        )
        .into());
    }
    if callback_count.load(Ordering::SeqCst) != 2 {
        return Err(format!(
            "the Rust callback should run once at initialization and once for the snapshot; got {} calls",
            callback_count.load(Ordering::SeqCst)
        )
        .into());
    }

    eprintln!(
        "Fabric boot passed: Loader {LOADER_VERSION}, Minecraft {GAME_VERSION}; Rust invoked Knot and Fabric entrypoints, and the test mod received one full JSON snapshot."
    );
    Ok(())
}

async fn system_property(vm: &JavaVm, name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = invoke_value(
        vm,
        "java.lang.System",
        "getProperty(Ljava/lang/String;)Ljava/lang/String;",
        &[name],
    )
    .await?;
    Ok(value.as_string()?.to_owned())
}

fn java_string_hash(value: &str) -> i32 {
    value.encode_utf16().fold(0_i32, |hash, character| {
        hash.wrapping_mul(31).wrapping_add(i32::from(character))
    })
}
