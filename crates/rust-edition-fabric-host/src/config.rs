use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use rust_edition_fabric::{Entrypoint, Limits, ModInfo, inspect_jar, inspect_jars};
use serde::Deserialize;
use serde_json::Value;
use zip::ZipArchive;

const CONFIG_VERSION: u8 = 1;
const GAME_VERSION: &str = "26.3";
const LOADER_VERSION: &str = "0.19.5";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_LIBRARY_JARS: usize = 1024;
const MAX_MOD_JARS: usize = 512;
const MAX_TOTAL_JAR_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    schema_version: u8,
    java_home: PathBuf,
    game_version: String,
    fabric_loader_jar: PathBuf,
    minecraft_client_jar: PathBuf,
    libraries: Vec<PathBuf>,
    mods: Vec<PathBuf>,
    callback_mod_id: String,
    callback_class: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackGroup {
    Main,
    Client,
}

#[derive(Debug)]
pub struct ValidatedConfig {
    pub java_home: PathBuf,
    pub fabric_loader_jar: PathBuf,
    pub minecraft_client_jar: PathBuf,
    pub libraries: Vec<PathBuf>,
    pub mods: Vec<PathBuf>,
    pub callback_mod_id: String,
    pub callback_class: String,
    pub callback_group: CallbackGroup,
}

impl ValidatedConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("config file path must be absolute".into());
        }
        let path = fs::canonicalize(path)
            .map_err(|error| format!("config file cannot be read: {error}"))?;
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("config file metadata cannot be read: {error}"))?;
        if !metadata.is_file() {
            return Err("config path is not a file".into());
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(format!("config is larger than {MAX_CONFIG_BYTES} bytes"));
        }
        let mut bytes =
            Vec::with_capacity((metadata.len() as usize).min(MAX_CONFIG_BYTES as usize));
        File::open(&path)
            .map_err(|error| format!("config file cannot be opened: {error}"))?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("config file cannot be read: {error}"))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(format!("config is larger than {MAX_CONFIG_BYTES} bytes"));
        }
        let config: HostConfig = serde_json::from_slice(&bytes)
            .map_err(|error| format!("config is not valid strict JSON: {error}"))?;
        config.validate()
    }
}

impl HostConfig {
    fn validate(self) -> Result<ValidatedConfig, String> {
        if self.schema_version != CONFIG_VERSION {
            return Err(format!(
                "config schema version must be {CONFIG_VERSION}, found {}",
                self.schema_version
            ));
        }
        if self.game_version != GAME_VERSION {
            return Err(format!(
                "game version must be {GAME_VERSION}, found {}",
                self.game_version
            ));
        }
        if self.libraries.len() > MAX_LIBRARY_JARS {
            return Err(format!("libraries has more than {MAX_LIBRARY_JARS} JARs"));
        }
        if self.mods.is_empty() {
            return Err("mods must contain at least one mod JAR".into());
        }
        if self.mods.len() > MAX_MOD_JARS {
            return Err(format!("mods has more than {MAX_MOD_JARS} JARs"));
        }
        if self.callback_mod_id.is_empty() || self.callback_mod_id.len() > 128 {
            return Err("callback_mod_id must contain 1 to 128 bytes".into());
        }
        validate_class_name(&self.callback_class)?;

        let java_home = absolute_directory(&self.java_home, "java_home")?;
        validate_java_home(&java_home)?;
        let fabric_loader_jar = absolute_jar(&self.fabric_loader_jar, "fabric_loader_jar")?;
        let minecraft_client_jar =
            absolute_jar(&self.minecraft_client_jar, "minecraft_client_jar")?;
        let libraries = canonical_jars(&self.libraries, "libraries")?;
        let mods = canonical_jars(&self.mods, "mods")?;

        let mut seen = HashSet::new();
        let mut total_bytes = 0_u64;
        for path in std::iter::once(&fabric_loader_jar)
            .chain(std::iter::once(&minecraft_client_jar))
            .chain(&libraries)
            .chain(&mods)
        {
            if !seen.insert(path.clone()) {
                return Err(format!(
                    "JAR path is listed more than once: {}",
                    path.display()
                ));
            }
            let bytes = fs::metadata(path)
                .map_err(|error| format!("could not inspect JAR {}: {error}", path.display()))?
                .len();
            total_bytes = total_bytes
                .checked_add(bytes)
                .ok_or_else(|| "total JAR size overflowed".to_owned())?;
            if total_bytes > MAX_TOTAL_JAR_BYTES {
                return Err(format!(
                    "configured JARs exceed the {MAX_TOTAL_JAR_BYTES}-byte total limit"
                ));
            }
        }

        let loader = inspect_jar(&fabric_loader_jar)
            .map_err(|error| format!("could not read Fabric Loader metadata: {error}"))?;
        let loader_mod = loader
            .mods
            .iter()
            .find(|module| module.id == "fabricloader")
            .ok_or("fabric_loader_jar does not declare the fabricloader mod ID")?;
        if loader_mod.version != LOADER_VERSION {
            return Err(format!(
                "Fabric Loader JAR must be {LOADER_VERSION}, found {}",
                loader_mod.version
            ));
        }
        let game_version = read_game_version(&minecraft_client_jar)?;
        if game_version != GAME_VERSION {
            return Err(format!(
                "Minecraft client JAR must be {GAME_VERSION}, found {game_version}"
            ));
        }

        let inspection = inspect_jars(&mods, Limits::default())
            .map_err(|error| format!("could not inspect configured mod JARs: {error}"))?;
        let callback_group = validate_callback_entrypoint(
            &inspection.mods,
            &inspection.mod_id_conflicts,
            &self.callback_mod_id,
            &self.callback_class,
        )?;

        Ok(ValidatedConfig {
            java_home,
            fabric_loader_jar,
            minecraft_client_jar,
            libraries,
            mods,
            callback_mod_id: self.callback_mod_id,
            callback_class: self.callback_class,
            callback_group,
        })
    }
}

fn validate_java_home(home: &Path) -> Result<(), String> {
    let release = fs::read_to_string(home.join("release"))
        .map_err(|error| format!("java_home has no readable release file: {error}"))?;
    let version = release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .map(|version| version.trim_matches('"'))
        .ok_or("java_home release file has no JAVA_VERSION")?;
    if version.split(['.', '+', '-']).next() != Some("25") {
        return Err(format!(
            "java_home must contain Java 25 libraries, found {version}"
        ));
    }
    if home.join("lib/modules").exists() {
        return Err("java_home must be a prepared JMOD library home without lib/modules".into());
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
        if !home.join("jmods").join(format!("{module}.jmod")).is_file() {
            return Err(format!("java_home is missing the {module} JMOD"));
        }
    }
    for file in ["lib/tzdb.dat", "conf/security/java.security"] {
        if !home.join(file).is_file() {
            return Err(format!("java_home is missing {file}"));
        }
    }
    Ok(())
}

fn canonical_jars(paths: &[PathBuf], label: &str) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| absolute_jar(path, &format!("{label}[{index}]")))
        .collect()
}

fn absolute_jar(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    let path =
        fs::canonicalize(path).map_err(|error| format!("{label} cannot be read: {error}"))?;
    if !path.is_file() {
        return Err(format!("{label} path is not a file"));
    }
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
    {
        return Err(format!("{label} must name a .jar file"));
    }
    Ok(path)
}

fn absolute_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    let path = fs::canonicalize(path)
        .map_err(|error| format!("{label} directory cannot be opened: {error}"))?;
    if !path.is_dir() {
        return Err(format!("{label} path is not a directory"));
    }
    Ok(path)
}

fn validate_class_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 512 || name.starts_with('.') || name.ends_with('.') {
        return Err("callback_class must be a dotted Java class name".into());
    }
    if name
        .split('.')
        .any(|part| part.is_empty() || part.contains('/') || part.contains(';'))
    {
        return Err("callback_class must be a dotted Java class name".into());
    }
    Ok(())
}

fn validate_callback_entrypoint(
    mods: &[ModInfo],
    conflicts: &[rust_edition_fabric::ModIdConflict],
    mod_id: &str,
    class_name: &str,
) -> Result<CallbackGroup, String> {
    if conflicts.iter().any(|conflict| conflict.id == mod_id) {
        return Err(format!(
            "callback mod ID {mod_id:?} has conflicting JAR contents"
        ));
    }
    let matches: Vec<_> = mods.iter().filter(|module| module.id == mod_id).collect();
    if matches.len() != 1 {
        return Err(format!(
            "callback mod ID {mod_id:?} must appear exactly once in the configured JARs; found {}",
            matches.len()
        ));
    }
    let mut declared_matches: Vec<(&str, &str, &Entrypoint)> = Vec::new();
    for module in mods {
        for (group, entries) in &module.entrypoints {
            for entry in entries {
                if entry.value.as_deref() == Some(class_name) {
                    declared_matches.push((module.id.as_str(), group.as_str(), entry));
                }
            }
        }
    }
    if declared_matches.len() != 1 || declared_matches[0].0 != mod_id {
        return Err(format!(
            "callback class {class_name:?} must be declared once under mod ID {mod_id:?}; found {} declaration(s)",
            declared_matches.len()
        ));
    }
    let (_, group, entry) = declared_matches.pop().expect("one matching entrypoint");
    if entry
        .adapter
        .as_deref()
        .is_some_and(|adapter| adapter != "default")
    {
        return Err(format!(
            "callback class {class_name:?} uses a language adapter, which this probe does not support"
        ));
    }
    match group {
        "main" => Ok(CallbackGroup::Main),
        "client" => Ok(CallbackGroup::Client),
        _ => Err(format!(
            "callback class {class_name:?} must use the main or client entrypoint group"
        )),
    }
}

fn read_game_version(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("could not open Minecraft JAR: {error}"))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("Minecraft client is not a readable JAR: {error}"))?;
    let entry = archive
        .by_name("version.json")
        .map_err(|error| format!("Minecraft client JAR has no version.json: {error}"))?;
    if entry.size() > 1024 * 1024 {
        return Err("Minecraft version.json is larger than 1 MiB".into());
    }
    let mut bytes = Vec::new();
    entry
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read Minecraft version.json: {error}"))?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Minecraft version.json is invalid: {error}"))?;
    json.get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Minecraft version.json has no string id".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_unknown_fields_and_wrong_schema_versions() {
        let unknown = r#"{"schema_version":1,"java_home":"/tmp/jdk","game_version":"26.3","fabric_loader_jar":"/tmp/loader.jar","minecraft_client_jar":"/tmp/game.jar","libraries":[],"mods":["/tmp/mod.jar"],"callback_mod_id":"probe","callback_class":"dev.rustedition.probe.TestMod","extra":true}"#;
        assert!(serde_json::from_str::<HostConfig>(unknown).is_err());

        let wrong_version = r#"{"schema_version":2,"java_home":"/tmp/jdk","game_version":"26.3","fabric_loader_jar":"/tmp/loader.jar","minecraft_client_jar":"/tmp/game.jar","libraries":[],"mods":["/tmp/mod.jar"],"callback_mod_id":"probe","callback_class":"dev.rustedition.probe.TestMod"}"#;
        let config: HostConfig = serde_json::from_str(wrong_version).expect("valid JSON shape");
        assert!(config.validate().is_err());
    }

    #[test]
    fn configured_paths_must_be_absolute_and_class_names_must_be_dotted() {
        assert!(absolute_jar(Path::new("relative.jar"), "mod").is_err());
        assert!(validate_class_name("dev.rustedition.probe.TestMod").is_ok());
        assert!(validate_class_name("dev/rustedition/probe/TestMod").is_err());
        assert!(validate_class_name("dev..probe.TestMod").is_err());
    }
}
