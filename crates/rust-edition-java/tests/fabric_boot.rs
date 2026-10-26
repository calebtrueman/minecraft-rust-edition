use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ristretto_vm::{Error as VmError, Parameters};
use rust_edition_java::{JavaVm, NativeMethod, NativeResult, Thread, Value};
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
import net.fabricmc.api.ModInitializer;

public final class TestMod implements ModInitializer {
    public static native int hostCall();

    @Override
    public void onInitialize() {
        int calls = hostCall();
        System.setProperty("probe.host_callback_result", Integer.toString(calls));
        System.setProperty("probe.test_mod_initialized", "yes");
    }
}
"#;

const BOOT_PROBE: &str = r#"
import java.util.List;
import net.fabricmc.api.EnvType;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.api.FabricLoader;
import net.fabricmc.loader.impl.launch.knot.Knot;

public final class BootProbe {
    public static int run() {
        try {
            System.setProperty("java.class.path", "JAVA_CLASS_PATH");
            System.setProperty("user.home", "GAME_DIR");
            System.setProperty("user.dir", "GAME_DIR");
            System.setProperty("java.io.tmpdir", "GAME_DIR/tmp");
            System.setProperty("fabric.gameVersion", "26.3");
            System.setProperty("fabric.gameJarPath", "GAME_JAR");
            System.setProperty("fabric.addMods", "MOD_JAR");
            System.setProperty("fabric.modsFolder", "GAME_DIR/mods");
            System.setProperty("fabric.log.file", "GAME_DIR/fabric.log");
            Knot knot = new Knot(EnvType.CLIENT);
            knot.init(new String[] {"--gameDir", "GAME_DIR", "--version", "26.3"});
            List<ModInitializer> entrypoints = FabricLoader.getInstance()
                    .getEntrypoints("main", ModInitializer.class);
            for (ModInitializer entrypoint : entrypoints) {
                entrypoint.onInitialize();
            }
            return 0;
        } catch (Throwable error) {
            StringBuilder detail = new StringBuilder();
            for (Throwable cause = error; cause != null; cause = cause.getCause()) {
                detail.append(cause.getClass().getName()).append(": ")
                        .append(String.valueOf(cause.getMessage())).append("\n");
                for (StackTraceElement frame : cause.getStackTrace()) {
                    detail.append("  at ").append(frame).append("\n");
                }
            }
            System.setProperty("probe.error", detail.toString());
            return -1;
        }
    }

    public static String error() {
        return System.getProperty("probe.error", "");
    }

    public static String marker() {
        return System.getProperty("probe.test_mod_initialized", "");
    }

    public static String callbackResult() {
        return System.getProperty("probe.host_callback_result", "");
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

#[tokio::test(flavor = "multi_thread")]
async fn fabric_loader_runs_a_mod_entrypoint_and_calls_a_rust_native_method()
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
    let classes_dir = scratch.0.join("probe-classes");
    let mod_classes = scratch.0.join("mod-classes");
    fs::create_dir_all(&mods_dir)?;
    fs::create_dir_all(&java_temp_dir)?;
    fs::create_dir_all(&classpath_dir)?;
    fs::create_dir_all(&classes_dir)?;
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
    fs::write(&mod_source, TEST_MOD)?;
    let compile_mod = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(&loader_jar)
        .arg("-d")
        .arg(&mod_classes)
        .arg(&mod_source)
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
            r#"{{"schemaVersion":1,"id":"probe_mod","version":"1.0.0","entrypoints":{{"main":["TestMod"]}},"depends":{{"fabricloader":">={LOADER_VERSION}","minecraft":"{GAME_VERSION}"}}}}"#
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
        classes_dir.clone(),
    ];
    let class_path = env::join_paths(&class_paths)?;
    let boot_probe_source = scratch.0.join("BootProbe.java");
    let boot_probe_source_text = BOOT_PROBE
        .replace("GAME_DIR", &java_string(&game_dir.to_string_lossy()))
        .replace("GAME_JAR", &java_string(&game_jar.to_string_lossy()))
        .replace("MOD_JAR", &java_string(&mod_jar.to_string_lossy()))
        .replace(
            "JAVA_CLASS_PATH",
            &java_string(&class_path.to_string_lossy()),
        );
    fs::write(&boot_probe_source, boot_probe_source_text)?;
    let compile_probe = Command::new(&javac)
        .args(["--release", "25", "-g:none", "-cp"])
        .arg(&class_path)
        .arg("-d")
        .arg(&classes_dir)
        .arg(&boot_probe_source)
        .output()?;
    if !compile_probe.status.success() {
        return Err(format!(
            "javac failed to build the boot probe: {}",
            String::from_utf8_lossy(&compile_probe.stderr)
        )
        .into());
    }

    let callback_count = Arc::new(AtomicUsize::new(0));
    let vm = JavaVm::new_with_host_context(
        &library_home,
        &class_paths,
        [NativeMethod::sync("TestMod.hostCall()I", host_call)],
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

    let boot_result = vm
        .invoke("BootProbe", "run()I", &[] as &[i32])
        .await?
        .ok_or("BootProbe.run() returned no value")?;
    if boot_result.as_i32()? != 0 {
        let error = vm
            .invoke("BootProbe", "error()Ljava/lang/String;", &[] as &[i32])
            .await?
            .ok_or("BootProbe.error() returned no value")?;
        return Err(format!("Fabric boot failed:\n{}", error.as_string()?).into());
    }

    let marker = probe_string(&vm, "marker()Ljava/lang/String;").await?;
    let callback_result = probe_string(&vm, "callbackResult()Ljava/lang/String;").await?;
    if marker != "yes" {
        let error = vm
            .invoke("BootProbe", "error()Ljava/lang/String;", &[] as &[i32])
            .await?
            .ok_or("BootProbe.error() returned no value")?;
        return Err(format!(
            "Fabric did not run TestMod.onInitialize(); marker was {marker:?}. Error: {}",
            error.as_string()?
        )
        .into());
    }
    if callback_result != "1" || callback_count.load(Ordering::SeqCst) != 1 {
        return Err(format!(
            "TestMod's Rust callback should return 1 and run once; marker result={callback_result:?}, callback count={}",
            callback_count.load(Ordering::SeqCst)
        )
        .into());
    }

    eprintln!(
        "Fabric boot passed: Loader {LOADER_VERSION}, Minecraft {GAME_VERSION}, test entrypoint called Rust once. Scratch files will be removed."
    );
    Ok(())
}

fn java_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn probe_string(vm: &JavaVm, method: &str) -> Result<String, Box<dyn std::error::Error>> {
    let result = vm
        .invoke("BootProbe", method, &[] as &[i32])
        .await?
        .ok_or_else(|| format!("BootProbe.{method} returned no string"))?;
    Ok(result.as_string()?.to_owned())
}
