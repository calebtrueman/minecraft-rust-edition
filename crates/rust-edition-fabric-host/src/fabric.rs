use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ristretto_vm::{Error as VmError, Parameters};
use rust_edition_java::{
    Error as JavaAdapterError, JavaVm, MethodExecutionPolicy, NativeMethod, NativeResult,
    RustValue, Thread, Value,
};
use rust_edition_mod_bridge::MAX_VALUE_BYTES;

use crate::config::{CallbackGroup, ValidatedConfig};

const MAX_ENTRYPOINTS: i32 = 16_384;

pub struct FabricRuntime {
    vm: JavaVm,
    callback_instance: Value,
    callback_class: String,
}

impl FabricRuntime {
    pub async fn start(config: &ValidatedConfig, scratch_dir: &Path) -> Result<Self, String> {
        let staged = StagedFiles::copy(config, scratch_dir)?;
        let class_path = std::env::join_paths(&staged.class_paths)
            .map_err(|error| format!("could not build the Java class path: {error}"))?
            .to_string_lossy()
            .into_owned();
        let game_dir_path = staged.game_dir.to_string_lossy().into_owned();
        let temp_dir_path = staged.temp_dir.to_string_lossy().into_owned();
        let game_jar_path = staged.game_jar.to_string_lossy().into_owned();
        let mods_dir_path = staged.mods_dir.to_string_lossy().into_owned();
        let fabric_log_path = staged.fabric_log.to_string_lossy().into_owned();
        let host_state = Arc::new(HostState::default());
        let native_signature = format!("{}.hostCall()I", config.callback_class.replace('.', "/"));
        let method_policy =
            MethodExecutionPolicy::deny_class_prefixes(["net/minecraft/", "com/mojang/blaze3d/"]);
        let vm = JavaVm::new_with_host_context_and_policy(
            &config.java_home,
            &staged.class_paths,
            [NativeMethod::sync(native_signature, host_call)],
            host_state,
            method_policy,
        )
        .await
        .map_err(|error| format!("could not start the Java 25 VM: {error}"))?;

        for (name, value) in [
            ("java.class.path", class_path.as_str()),
            ("user.home", game_dir_path.as_str()),
            ("user.dir", game_dir_path.as_str()),
            ("java.io.tmpdir", temp_dir_path.as_str()),
            ("fabric.gameVersion", "26.3"),
            ("fabric.gameJarPath", game_jar_path.as_str()),
            ("fabric.modsFolder", mods_dir_path.as_str()),
            ("fabric.log.file", fabric_log_path.as_str()),
        ] {
            set_property(&vm, name, value).await?;
        }

        let environment = invoke_value(
            &vm,
            "net.fabricmc.api.EnvType",
            "valueOf(Ljava/lang/String;)Lnet/fabricmc/api/EnvType;",
            &["CLIENT"],
        )
        .await?;
        let knot = new_java_object(
            &vm,
            "net.fabricmc.loader.impl.launch.knot.Knot",
            "Lnet/fabricmc/api/EnvType;",
            &[environment],
        )
        .await?;
        let arguments = string_array(
            &vm,
            &["--gameDir", game_dir_path.as_str(), "--version", "26.3"],
        )
        .await?;
        let knot_arguments = [knot.clone(), arguments];
        invoke_java(
            &vm,
            "net.fabricmc.loader.impl.launch.knot.Knot",
            "init([Ljava/lang/String;)Ljava/lang/ClassLoader;",
            &knot_arguments,
        )
        .await?;

        let loader = invoke_value(
            &vm,
            "net.fabricmc.loader.api.FabricLoader",
            "getInstance()Lnet/fabricmc/loader/api/FabricLoader;",
            &[] as &[i32],
        )
        .await?;
        let main_entrypoints =
            get_entrypoint_containers(&vm, &loader, "main", "net.fabricmc.api.ModInitializer")
                .await?;

        let mut callback = None;
        let mut callback_matches = 0;
        for entrypoint in &main_entrypoints {
            let runtime_class = runtime_class_name(&vm, &entrypoint.instance).await?;
            if runtime_class == config.callback_class {
                if entrypoint.mod_id != config.callback_mod_id {
                    return Err(format!(
                        "callback class {} was loaded from mod {}, expected {}",
                        config.callback_class, entrypoint.mod_id, config.callback_mod_id
                    ));
                }
                callback_matches += 1;
                if config.callback_group != CallbackGroup::Main {
                    return Err(format!(
                        "callback class {} is registered in metadata as client but loaded as main",
                        config.callback_class
                    ));
                }
                callback = Some((entrypoint.instance.clone(), runtime_class));
            }
            invoke_java(
                &vm,
                "net.fabricmc.api.ModInitializer",
                "onInitialize()V",
                std::slice::from_ref(&entrypoint.instance),
            )
            .await
            .map_err(|error| format!("a Fabric main entrypoint failed: {error}"))?;
        }

        let client_entrypoints = get_entrypoint_containers(
            &vm,
            &loader,
            "client",
            "net.fabricmc.api.ClientModInitializer",
        )
        .await?;
        for entrypoint in &client_entrypoints {
            let runtime_class = runtime_class_name(&vm, &entrypoint.instance).await?;
            if runtime_class == config.callback_class {
                if entrypoint.mod_id != config.callback_mod_id {
                    return Err(format!(
                        "callback class {} was loaded from mod {}, expected {}",
                        config.callback_class, entrypoint.mod_id, config.callback_mod_id
                    ));
                }
                callback_matches += 1;
                if config.callback_group != CallbackGroup::Client {
                    return Err(format!(
                        "callback class {} is registered in metadata as main but loaded as client",
                        config.callback_class
                    ));
                }
                callback = Some((entrypoint.instance.clone(), runtime_class));
            }
            invoke_java(
                &vm,
                "net.fabricmc.api.ClientModInitializer",
                "onInitializeClient()V",
                std::slice::from_ref(&entrypoint.instance),
            )
            .await
            .map_err(|error| format!("a Fabric client entrypoint failed: {error}"))?;
        }
        if callback_matches != 1 {
            return Err(format!(
                "configured callback {} from mod {} must load as exactly one Fabric entrypoint; found {callback_matches}",
                config.callback_class, config.callback_mod_id
            ));
        }
        let (callback_instance, runtime_class) =
            callback.ok_or_else(|| "configured callback was not loaded".to_owned())?;
        validate_callback_method(&vm, &runtime_class, &callback_instance).await?;

        Ok(Self {
            vm,
            callback_instance,
            callback_class: runtime_class,
        })
    }

    pub async fn on_snapshot(&self, snapshot_json: &str) -> Result<Option<String>, String> {
        let java_snapshot = java_string(&self.vm, snapshot_json).await?;
        let arguments = [self.callback_instance.clone(), java_snapshot];
        let result = invoke_java(
            &self.vm,
            &self.callback_class,
            "onSnapshot(Ljava/lang/String;)Ljava/lang/String;",
            &arguments,
        )
        .await?
        .ok_or("Fabric snapshot callback returned no value")?;
        if result.is_null() {
            return Ok(None);
        }
        result
            .as_string()
            .map(|value| Some(value.to_owned()))
            .map_err(|error| {
                format!("Fabric snapshot callback did not return a Java String: {error}")
            })
    }
}

struct StagedFiles {
    game_dir: PathBuf,
    mods_dir: PathBuf,
    temp_dir: PathBuf,
    fabric_log: PathBuf,
    game_jar: PathBuf,
    class_paths: Vec<PathBuf>,
}

struct LoadedEntrypoint {
    mod_id: String,
    instance: Value,
}

impl StagedFiles {
    fn copy(config: &ValidatedConfig, scratch_dir: &Path) -> Result<Self, String> {
        if !scratch_dir.is_absolute() {
            return Err("helper scratch directory must be absolute".into());
        }
        let scratch_dir = fs::canonicalize(scratch_dir)
            .map_err(|error| format!("helper scratch directory cannot be opened: {error}"))?;
        if !scratch_dir.is_dir() {
            return Err("helper scratch path is not a directory".into());
        }
        let game_dir = scratch_dir.join("fabric-host");
        let class_dir = game_dir.join("classpath");
        let mods_dir = game_dir.join("mods");
        let temp_dir = game_dir.join("tmp");
        fs::create_dir_all(&class_dir)
            .and_then(|()| fs::create_dir_all(&mods_dir))
            .and_then(|()| fs::create_dir_all(&temp_dir))
            .map_err(|error| format!("could not prepare the helper's scratch folders: {error}"))?;

        let loader = copy_jar(
            &config.fabric_loader_jar,
            &class_dir.join("fabric-loader.jar"),
        )?;
        let game_jar = copy_jar(
            &config.minecraft_client_jar,
            &class_dir.join("minecraft-client.jar"),
        )?;
        let mut class_paths = vec![loader, game_jar.clone()];
        for (index, path) in config.libraries.iter().enumerate() {
            class_paths.push(copy_jar(
                path,
                &class_dir.join(format!("library-{index:04}.jar")),
            )?);
        }
        for (index, path) in config.mods.iter().enumerate() {
            let path = copy_jar(path, &mods_dir.join(format!("mod-{index:04}.jar")))?;
            class_paths.push(path);
        }

        Ok(Self {
            game_dir: game_dir.clone(),
            mods_dir,
            temp_dir,
            fabric_log: game_dir.join("fabric.log"),
            game_jar,
            class_paths,
        })
    }
}

fn copy_jar(source: &Path, destination: &Path) -> Result<PathBuf, String> {
    let copied = fs::copy(source, destination)
        .map_err(|error| format!("could not copy configured JAR into scratch: {error}"))?;
    let expected = fs::metadata(source)
        .map_err(|error| format!("could not read configured JAR size: {error}"))?
        .len();
    if copied != expected {
        return Err(format!(
            "copy of configured JAR was incomplete ({copied} of {expected} bytes)"
        ));
    }
    Ok(destination.to_path_buf())
}

async fn set_property(vm: &JavaVm, name: &str, value: &str) -> Result<(), String> {
    invoke_java(
        vm,
        "java.lang.System",
        "setProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[name, value],
    )
    .await?;
    Ok(())
}

async fn get_entrypoint_containers(
    vm: &JavaVm,
    loader: &Value,
    group: &str,
    interface_name: &str,
) -> Result<Vec<LoadedEntrypoint>, String> {
    let group = java_string(vm, group).await?;
    let interface = class_object(vm, interface_name).await?;
    let arguments = [loader.clone(), group, interface];
    let list = invoke_value(
        vm,
        "net.fabricmc.loader.api.FabricLoader",
        "getEntrypointContainers(Ljava/lang/String;Ljava/lang/Class;)Ljava/util/List;",
        &arguments,
    )
    .await?;
    let count = invoke_value(vm, "java.util.List", "size()I", std::slice::from_ref(&list))
        .await?
        .as_i32()
        .map_err(|error| format!("Fabric returned an invalid entrypoint count: {error}"))?;
    if !(0..=MAX_ENTRYPOINTS).contains(&count) {
        return Err(format!(
            "Fabric returned an invalid entrypoint count: {count}"
        ));
    }
    let mut entrypoints = Vec::with_capacity(count as usize);
    for index in 0..count {
        let arguments = [list.clone(), Value::Int(index)];
        let container =
            invoke_value(vm, "java.util.List", "get(I)Ljava/lang/Object;", &arguments).await?;
        let provider = invoke_value(
            vm,
            "net.fabricmc.loader.api.entrypoint.EntrypointContainer",
            "getProvider()Lnet/fabricmc/loader/api/ModContainer;",
            std::slice::from_ref(&container),
        )
        .await?;
        let metadata = invoke_value(
            vm,
            "net.fabricmc.loader.api.ModContainer",
            "getMetadata()Lnet/fabricmc/loader/api/metadata/ModMetadata;",
            std::slice::from_ref(&provider),
        )
        .await?;
        let mod_id = invoke_value(
            vm,
            "net.fabricmc.loader.api.metadata.ModMetadata",
            "getId()Ljava/lang/String;",
            std::slice::from_ref(&metadata),
        )
        .await?
        .as_string()
        .map_err(|error| format!("Fabric returned an invalid mod ID: {error}"))?;
        let instance = invoke_value(
            vm,
            "net.fabricmc.loader.api.entrypoint.EntrypointContainer",
            "getEntrypoint()Ljava/lang/Object;",
            std::slice::from_ref(&container),
        )
        .await?;
        entrypoints.push(LoadedEntrypoint { mod_id, instance });
    }
    Ok(entrypoints)
}

async fn runtime_class_name(vm: &JavaVm, instance: &Value) -> Result<String, String> {
    let class = invoke_value(
        vm,
        "java.lang.Object",
        "getClass()Ljava/lang/Class;",
        std::slice::from_ref(instance),
    )
    .await?;
    let name = invoke_value(
        vm,
        "java.lang.Class",
        "getName()Ljava/lang/String;",
        std::slice::from_ref(&class),
    )
    .await?;
    name.as_string()
        .map_err(|error| format!("could not read Fabric entrypoint class name: {error}"))
}

async fn validate_callback_method(
    vm: &JavaVm,
    class_name: &str,
    instance: &Value,
) -> Result<(), String> {
    let class = invoke_value(
        vm,
        "java.lang.Object",
        "getClass()Ljava/lang/Class;",
        std::slice::from_ref(instance),
    )
    .await?;
    let method_name = java_string(vm, "onSnapshot").await?;
    let string_class = class_object(vm, "java.lang.String").await?;
    let parameter_types = reference_array(vm, "java.lang.Class", &[string_class]).await?;
    let arguments = [class, method_name, parameter_types];
    let method = invoke_value(
        vm,
        "java.lang.Class",
        "getMethod(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &arguments,
    )
    .await
    .map_err(|error| {
        format!(
            "configured callback {class_name}.onSnapshot(String) is missing or not public: {error}"
        )
    })?;
    let return_type = invoke_value(
        vm,
        "java.lang.reflect.Method",
        "getReturnType()Ljava/lang/Class;",
        std::slice::from_ref(&method),
    )
    .await?;
    let return_type = invoke_value(
        vm,
        "java.lang.Class",
        "getName()Ljava/lang/String;",
        std::slice::from_ref(&return_type),
    )
    .await?
    .as_string()
    .map_err(|error| format!("could not inspect callback return type: {error}"))?;
    if return_type != "java.lang.String" {
        return Err(format!(
            "configured callback {class_name}.onSnapshot(String) must return java.lang.String, found {return_type}"
        ));
    }
    let modifiers = invoke_value(
        vm,
        "java.lang.reflect.Method",
        "getModifiers()I",
        std::slice::from_ref(&method),
    )
    .await?
    .as_i32()
    .map_err(|error| format!("could not inspect callback modifiers: {error}"))?;
    const PUBLIC: i32 = 0x0001;
    const STATIC: i32 = 0x0008;
    if modifiers & PUBLIC == 0 || modifiers & STATIC != 0 {
        return Err(format!(
            "configured callback {class_name}.onSnapshot(String) must be a public instance method"
        ));
    }
    Ok(())
}

async fn class_object(vm: &JavaVm, class_name: &str) -> Result<Value, String> {
    invoke_value(
        vm,
        "java.lang.Class",
        "forName(Ljava/lang/String;)Ljava/lang/Class;",
        &[class_name],
    )
    .await
}

async fn java_string(vm: &JavaVm, value: &str) -> Result<Value, String> {
    new_java_object(vm, "java.lang.String", "Ljava/lang/String;", &[value]).await
}

async fn string_array(vm: &JavaVm, values: &[&str]) -> Result<Value, String> {
    let mut objects = Vec::with_capacity(values.len());
    for value in values {
        objects.push(java_string(vm, value).await?);
    }
    reference_array(vm, "java.lang.String", &objects).await
}

async fn reference_array(
    vm: &JavaVm,
    element_type: &str,
    values: &[Value],
) -> Result<Value, String> {
    let element_class = class_object(vm, element_type).await?;
    let array = invoke_value(
        vm,
        "java.lang.reflect.Array",
        "newInstance(Ljava/lang/Class;I)Ljava/lang/Object;",
        &[
            element_class,
            Value::Int(i32::try_from(values.len()).map_err(|error| error.to_string())?),
        ],
    )
    .await?;
    for (index, value) in values.iter().enumerate() {
        let arguments = [
            array.clone(),
            Value::Int(i32::try_from(index).map_err(|error| error.to_string())?),
            value.clone(),
        ];
        invoke_java(
            vm,
            "java.lang.reflect.Array",
            "set(Ljava/lang/Object;ILjava/lang/Object;)V",
            &arguments,
        )
        .await
        .map_err(|error| format!("could not fill a Java reflection array: {error}"))?;
    }
    Ok(array)
}

async fn invoke_value(
    vm: &JavaVm,
    class_name: &str,
    method_signature: &str,
    arguments: &[impl RustValue],
) -> Result<Value, String> {
    invoke_java(vm, class_name, method_signature, arguments)
        .await?
        .ok_or_else(|| format!("{class_name}.{method_signature} returned no value"))
}

async fn invoke_java(
    vm: &JavaVm,
    class_name: &str,
    method_signature: &str,
    arguments: &[impl RustValue],
) -> Result<Option<Value>, String> {
    let context = format!("{class_name}.{method_signature} failed");
    match vm.invoke(class_name, method_signature, arguments).await {
        Ok(value) => Ok(value),
        Err(error) => Err(format_java_failure(vm, &context, error).await),
    }
}

async fn new_java_object(
    vm: &JavaVm,
    class_name: &str,
    parameter_descriptor: &str,
    arguments: &[impl RustValue],
) -> Result<Value, String> {
    let context = format!("could not create Java object {class_name}");
    match vm
        .new_object(class_name, parameter_descriptor, arguments)
        .await
    {
        Ok(value) => Ok(value),
        Err(error) => Err(format_java_failure(vm, &context, error).await),
    }
}

const MAX_THROWABLE_CAUSE_DEPTH: usize = 8;
const MAX_THROWABLE_MESSAGE_BYTES: usize = 1_024;
const MAX_DEEPEST_CAUSE_BYTES: usize = 150;
const MAX_FAILURE_CONTEXT_BYTES: usize = 56;

struct ThrowableCause {
    class_name: String,
    message: Option<String>,
}

struct ThrowableCauseChain {
    causes: Vec<ThrowableCause>,
    ended: bool,
    cycle: bool,
    truncated: bool,
}

async fn format_java_failure(vm: &JavaVm, context: &str, error: JavaAdapterError) -> String {
    let fallback_message = format!("{context}: {error}");
    let fallback = bound_utf8(&fallback_message, MAX_VALUE_BYTES).to_owned();
    let throwable = match &error {
        JavaAdapterError::Ristretto(error) => match error.as_ref() {
            VmError::Throwable(throwable) => Some(throwable),
            _ => None,
        },
        JavaAdapterError::Configuration(_) => None,
    };
    let Some(throwable) = throwable else {
        return fallback;
    };
    let Some(chain) = throwable_cause_chain(vm, throwable).await else {
        return fallback;
    };
    let Some(deepest) = chain.causes.last() else {
        return fallback;
    };

    let label = if chain.ended {
        "root cause"
    } else {
        "deepest cause seen"
    };
    let mut deepest_text = deepest.class_name.clone();
    if let Some(message) = &deepest.message
        && !message.is_empty()
    {
        deepest_text.push_str(": ");
        deepest_text.push_str(message);
    }
    let deepest_text = bound_utf8(&deepest_text, MAX_DEEPEST_CAUSE_BYTES);
    let mut detail = format!("{label}: {deepest_text}");
    if chain.cycle {
        detail.push_str(" (cause cycle stopped)");
    } else if chain.truncated {
        detail.push_str(" (cause chain limit reached)");
    }
    if chain.causes.len() > 1
        && let Some(outer) = chain.causes.first()
    {
        let outer_name = outer
            .class_name
            .rsplit('.')
            .next()
            .unwrap_or(outer.class_name.as_str());
        detail.push_str(&format!(" (wrapped by {outer_name})"));
    }

    // Keep the deepest Java message even when the 256-byte bridge limit leaves little room.
    let detail = bound_utf8(
        &detail,
        MAX_VALUE_BYTES.saturating_sub(MAX_FAILURE_CONTEXT_BYTES + 2),
    );
    let context_limit = MAX_VALUE_BYTES.saturating_sub(detail.len() + 2);
    let context_limit = context_limit.min(MAX_FAILURE_CONTEXT_BYTES);
    format!("{}: {detail}", tail_utf8(context, context_limit))
}

async fn throwable_cause_chain(vm: &JavaVm, root: &Value) -> Option<ThrowableCauseChain> {
    let mut current = root.clone();
    let mut seen = Vec::with_capacity(MAX_THROWABLE_CAUSE_DEPTH);
    let mut causes = Vec::with_capacity(MAX_THROWABLE_CAUSE_DEPTH);
    let mut ended = false;
    let mut cycle = false;
    let mut truncated = false;

    for depth in 0..MAX_THROWABLE_CAUSE_DEPTH {
        if seen
            .iter()
            .any(|previous| vm.same_object(previous, &current))
        {
            cycle = true;
            break;
        }
        seen.push(current.clone());

        let class_name = match &current {
            Value::Object(Some(reference)) => reference.read().class_name().ok()?.replace('/', "."),
            _ => return None,
        };
        let arguments = [current.clone()];
        let message_value = vm
            .invoke(
                "java.lang.Throwable",
                "getMessage()Ljava/lang/String;",
                &arguments,
            )
            .await
            .ok()??;
        let message = match message_value {
            Value::Object(None) => None,
            Value::Object(Some(_)) => {
                let message = message_value.as_string().ok()?;
                Some(safe_java_text(&message, MAX_THROWABLE_MESSAGE_BYTES))
            }
            _ => return None,
        };
        let cause_value = vm
            .invoke(
                "java.lang.Throwable",
                "getCause()Ljava/lang/Throwable;",
                &arguments,
            )
            .await
            .ok()??;
        causes.push(ThrowableCause {
            class_name,
            message,
        });

        match cause_value {
            Value::Object(None) => {
                ended = true;
                break;
            }
            Value::Object(Some(_)) => {
                if seen
                    .iter()
                    .any(|previous| vm.same_object(previous, &cause_value))
                {
                    cycle = true;
                    break;
                }
                if depth + 1 == MAX_THROWABLE_CAUSE_DEPTH {
                    truncated = true;
                    break;
                }
                current = cause_value;
            }
            _ => return None,
        }
    }

    Some(ThrowableCauseChain {
        causes,
        ended,
        cycle,
        truncated,
    })
}

fn safe_java_text(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > maximum {
            break;
        }
        output.push(character);
    }
    output.trim().to_owned()
}

fn bound_utf8(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn tail_utf8(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut start = value.len() - maximum;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[derive(Default)]
struct HostState {
    native_calls: AtomicUsize,
}

#[allow(
    clippy::result_large_err,
    reason = "the native callback must use Ristretto's declared result type"
)]
fn host_call(thread: Arc<Thread>, _parameters: Parameters) -> NativeResult<Option<Value>> {
    let state = thread
        .vm()?
        .host_context()
        .ok_or_else(|| VmError::InternalError("Fabric callback has no host state".into()))?;
    let state = Arc::downcast::<HostState>(state).map_err(|_| {
        VmError::InternalError("Fabric callback host state has the wrong type".into())
    })?;
    let result = state.native_calls.fetch_add(1, Ordering::SeqCst) + 1;
    let result = i32::try_from(result)
        .map_err(|error| VmError::InternalError(format!("callback count is too large: {error}")))?;
    Ok(Some(Value::Int(result)))
}
