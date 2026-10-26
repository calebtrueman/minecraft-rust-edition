//! Read an existing Minecraft Launcher or Prism session without changing its files.
//! Microsoft refresh tokens and application IDs are deliberately never imported.
use super::*;
use chrono::DateTime;
use std::io::Read;

const PRISM_EXPIRED: &str = "Your existing Minecraft session has expired. Open Prism Launcher, launch Minecraft once while online, then try again.";
const OFFICIAL_EXPIRED: &str = "Your Minecraft Launcher session has expired. Sign in and launch Minecraft once while online, then try again.";
const NO_REUSABLE_SESSION: &str = "The source launcher did not expose a reusable Minecraft session. Sign in and launch Minecraft once while online, then try again.";
const OFFICIAL_GAME_NOT_RUNNING: &str = "The Minecraft Launcher did not expose a reusable session. Sign in and keep Minecraft running, then try again.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Prism,
    Official,
}

#[derive(Debug)]
struct SessionData {
    uuid: String,
    token: Option<String>,
    expires: u64,
    source: Source,
}

fn read_account_file(path: &Path) -> Result<Value, String> {
    let file = std::fs::File::open(path)
        .map_err(|_| "Could not read the launcher's account file.".to_owned())?;
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the existing launcher session.".to_owned())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("The account file is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The launcher's account file is not valid JSON.".into())
}

fn normalized_uuid(uuid: &str) -> Option<String> {
    let uuid = uuid.replace('-', "").to_ascii_lowercase();
    (uuid.len() == 32 && uuid.bytes().all(|b| b.is_ascii_hexdigit())).then_some(uuid)
}

fn uuid_matches(value: &Value, profile_key: &str, expected: Option<&str>) -> bool {
    let Some(expected) = expected.and_then(normalized_uuid) else {
        return false;
    };
    value
        .get(profile_key)
        .and_then(|profile| profile.get("id"))
        .and_then(Value::as_str)
        .and_then(normalized_uuid)
        .is_some_and(|uuid| uuid == expected)
}

fn parse_session_data(v: &Value, expected: Option<&str>, time: u64) -> Result<SessionData, String> {
    match v.get("accounts") {
        Some(Value::Array(accounts)) => {
            let selected = accounts
                .iter()
                .find(|account| {
                    expected.map_or_else(
                        || account["active"].as_bool() == Some(true),
                        |uuid| uuid_matches(account, "profile", Some(uuid)),
                    )
                })
                .ok_or(
                    "No selected account was found. Select your account in Prism Launcher first.",
                )?;
            let uuid = selected["profile"]["id"]
                .as_str()
                .and_then(normalized_uuid)
                .ok_or("The existing account has no valid Minecraft profile ID.")?;
            let expires = selected["ygg"]["exp"].as_u64().ok_or(PRISM_EXPIRED)?;
            if expires <= time.saturating_add(60) {
                return Err(PRISM_EXPIRED.into());
            }
            Ok(SessionData {
                uuid,
                token: selected["ygg"]["token"]
                    .as_str()
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
                expires,
                source: Source::Prism,
            })
        }
        Some(Value::Object(accounts)) => {
            let selected = if let Some(expected) = expected {
                accounts
                    .values()
                    .find(|account| uuid_matches(account, "minecraftProfile", Some(expected)))
            } else {
                v["activeAccountLocalId"]
                    .as_str()
                    .and_then(|local_id| accounts.get(local_id))
            }
            .ok_or("No selected Xbox account was found in the Minecraft Launcher.")?;
            if !selected["type"]
                .as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("Xbox"))
            {
                return Err(
                    "The selected Minecraft Launcher account is not an Xbox account.".into(),
                );
            }
            let uuid = selected["minecraftProfile"]["id"]
                .as_str()
                .and_then(normalized_uuid)
                .ok_or("The Minecraft Launcher account has no valid Minecraft profile ID.")?;
            let expires_at = selected["accessTokenExpiresAt"]
                .as_str()
                .ok_or(OFFICIAL_EXPIRED)?;
            let expires = DateTime::parse_from_rfc3339(expires_at)
                .ok()
                .and_then(|date| u64::try_from(date.timestamp()).ok())
                .ok_or(OFFICIAL_EXPIRED)?;
            if expires <= time.saturating_add(60) {
                return Err(OFFICIAL_EXPIRED.into());
            }
            Ok(SessionData {
                uuid,
                token: selected["accessToken"]
                    .as_str()
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
                expires,
                source: Source::Official,
            })
        }
        _ => Err(
            "Choose a Minecraft Launcher launcher_accounts.json or Prism accounts.json file."
                .into(),
        ),
    }
}

fn secrets(path: &Path, data: SessionData, token: String) -> Secrets {
    Secrets {
        refresh_token: String::new(),
        imported_from: Some(path.to_owned()),
        minecraft_token: token,
        minecraft_expires_at: data.expires,
        xuid: None,
    }
}

fn session_token(
    data: &SessionData,
    running_token: Option<String>,
    cached: Option<(&str, u64)>,
    time: u64,
) -> Result<(String, u64), String> {
    if let Some(token) = &data.token {
        return Ok((token.clone(), data.expires));
    }
    if data.source == Source::Official {
        if let Some(token) = running_token {
            return Ok((token, data.expires));
        }
        if let Some((token, expires)) = cached {
            if !token.trim().is_empty() && expires > time.saturating_add(60) {
                // The official file can report a new expiry, but it cannot extend the lifetime
                // of the token already cached by Horizon.
                return Ok((token.to_owned(), data.expires.min(expires)));
            }
            return Err(OFFICIAL_EXPIRED.into());
        }
        return Err(OFFICIAL_GAME_NOT_RUNNING.into());
    }
    Err(NO_REUSABLE_SESSION.into())
}

#[cfg(test)]
fn parse_session(
    v: &Value,
    path: &Path,
    expected: Option<&str>,
    time: u64,
) -> Result<(String, Secrets), String> {
    let data = parse_session_data(v, expected, time)?;
    let token = data.token.clone().ok_or(NO_REUSABLE_SESSION)?;
    Ok((data.uuid.clone(), secrets(path, data, token)))
}

pub(super) fn read_session(
    ep: &Endpoints,
    path: &Path,
    expected: Option<&str>,
) -> Result<(Account, Secrets), String> {
    read_session_with_cached(ep, path, expected, None)
}

pub(super) fn read_session_with_cached(
    ep: &Endpoints,
    path: &Path,
    expected: Option<&str>,
    cached: Option<(&str, u64)>,
) -> Result<(Account, Secrets), String> {
    read_session_using(ep, path, expected, cached, running_official_game_token)
}

pub(super) fn read_session_using(
    ep: &Endpoints,
    path: &Path,
    expected: Option<&str>,
    cached: Option<(&str, u64)>,
    find_running_token: impl FnOnce(&str) -> Option<String>,
) -> Result<(Account, Secrets), String> {
    let data = parse_session_data(&read_account_file(path)?, expected, now())?;
    let running_token = if data.source == Source::Official && data.token.is_none() {
        find_running_token(&data.uuid)
    } else {
        None
    };
    let (token, expires) = session_token(&data, running_token, cached, now())?;
    let data = SessionData { expires, ..data };
    let uuid = data.uuid.clone();
    let secrets = secrets(path, data, token);
    // Ask Minecraft, rather than trusting the saved name, UUID or entitlement flag.
    if !owns_game(ep, &secrets.minecraft_token)? {
        return Err("This session does not show ownership of Minecraft: Java Edition.".into());
    }
    let account = profile(ep, &secrets.minecraft_token)?;
    if normalized_uuid(&account.uuid).as_deref() != Some(uuid.as_str()) {
        return Err("The saved session belongs to a different Minecraft account. Sign in again in the source launcher.".into());
    }
    Ok((account, secrets))
}

pub fn import_from(ep: &Endpoints, path: &Path) -> Result<Account, String> {
    let path = path
        .canonicalize()
        .map_err(|_| "Could not find the existing launcher's account file.".to_owned())?;
    let (account, secrets) = read_session(ep, &path, None)?;
    let mut accounts = Accounts::load();
    accounts.add(store(), account.clone(), &secrets)?;
    accounts.save()?;
    Ok(account)
}

pub fn import_existing(ep: &Endpoints) -> Result<Account, String> {
    let folders =
        crate::sources::Folders::current().ok_or("Could not find your launcher folders.")?;
    let known = crate::sources::known_folders(crate::sources::Os::current(), &folders);
    import_from(ep, &default_account_file(&known)?)
}

fn default_account_file(known: &[(&'static str, PathBuf)]) -> Result<PathBuf, String> {
    // The official launcher's file takes precedence. If it exists but has no usable session, keep
    // that error visible instead of silently importing a different account from Prism.
    if let Some(path) = known
        .iter()
        .filter(|(name, _)| *name == "the official launcher")
        .map(|(_, folder)| folder.join("launcher_accounts.json"))
        .find(|path| path.is_file())
    {
        return Ok(path);
    }
    let paths: Vec<_> = known
        .iter()
        .filter(|(name, _)| *name == "Prism Launcher")
        .map(|(_, folder)| folder.join("accounts.json"))
        .filter(|path| path.is_file())
        .collect();
    if paths.len() > 1 {
        return Err("Several Prism Launcher installs were found. Use `horizon-launcher account import PATH` to choose their accounts.json file.".into());
    }
    paths.first().cloned().ok_or_else(|| {
        "No Minecraft Launcher or Prism account file was found. Use `horizon-launcher account import PATH` to choose a launcher account file.".into()
    })
}

/// Read the running official Minecraft game's token only when its account file contains no token.
/// The process list is never printed or logged. On macOS, inspect only Java process arguments and
/// accept a token from the official game only when its `--uuid` matches the account being read.
fn running_official_game_token(expected_uuid: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let list = std::process::Command::new("ps")
            .args(["-axo", "pid=,comm="])
            .output()
            .ok()?;
        if !list.status.success() {
            return None;
        }
        for line in String::from_utf8_lossy(&list.stdout).lines() {
            let Some((pid, command)) = java_pid_and_command(line) else {
                continue;
            };
            let Ok(pid) = pid.parse::<u32>() else {
                continue;
            };
            if command != "java" && !command.ends_with("/java") {
                continue;
            }
            let pid = pid.to_string();
            let Ok(args) = std::process::Command::new("ps")
                .args(["-p", pid.as_str(), "-o", "args="])
                .output()
            else {
                continue;
            };
            if !args.status.success() {
                continue;
            }
            if let Some(token) =
                token_from_game_args(&String::from_utf8_lossy(&args.stdout), expected_uuid)
            {
                return Some(token);
            }
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = expected_uuid;
        None
    }
}

fn java_pid_and_command(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    let end = line.find(char::is_whitespace)?;
    let pid = &line[..end];
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((pid, line[end..].trim()))
}

fn token_from_game_args(args: &str, expected_uuid: &str) -> Option<String> {
    let args: Vec<_> = args.split_whitespace().collect();
    if !args.contains(&"net.minecraft.client.main.Main") {
        return None;
    }
    let value = |flag: &str| {
        args.iter().enumerate().find_map(|(index, arg)| {
            arg.strip_prefix(&format!("{flag}=")).or_else(|| {
                (*arg == flag)
                    .then(|| args.get(index + 1).copied())
                    .flatten()
            })
        })
    };
    if normalized_uuid(value("--uuid")?)? != normalized_uuid(expected_uuid)? {
        return None;
    }
    let token = value("--accessToken")?.trim();
    if token.is_empty()
        || token == "0"
        || token.eq_ignore_ascii_case("null")
        || token.eq_ignore_ascii_case("undefined")
        || token.contains("${")
        || token.starts_with('<')
    {
        return None;
    }
    Some(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    const UUID: &str = "069a79f444e94726a5befca90e38aaf5";
    const OTHER_UUID: &str = "00000000000000000000000000000001";
    fn fixture() -> Value {
        json!({"accounts": [{"active": true, "profile": {"id": UUID},
            "msa": {"refresh_token": "must-not-import"}, "msa-client-id": "must-not-import",
            "ygg": {"token": "minecraft-only", "exp": 10000}}]})
    }

    fn official_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/official-launcher-accounts.json"
        ))
        .unwrap()
    }

    #[test]
    fn imports_only_minecraft_session_and_hides_secrets() {
        let (_, secrets) =
            parse_session(&fixture(), Path::new("accounts.json"), None, 100).unwrap();
        assert_eq!(secrets.minecraft_token, "minecraft-only");
        assert!(secrets.refresh_token.is_empty());
        assert!(!format!("{secrets:?}").contains("minecraft-only"));
        assert!(!serde_json::to_string(&secrets)
            .unwrap()
            .contains("must-not-import"));
    }
    #[test]
    fn refuses_expired_missing_and_different_accounts() {
        assert!(parse_session(&fixture(), Path::new("a"), None, 9990).is_err());
        let mut v = fixture();
        v["accounts"][0]["active"] = json!(false);
        assert!(parse_session(&v, Path::new("a"), None, 100).is_err());
        assert!(parse_session(&v, Path::new("a"), Some(UUID), 100).is_ok());
        assert!(parse_session(&v, Path::new("a"), Some("another-account"), 100).is_err());
        v["accounts"][0]["ygg"]["token"] = json!("");
        assert!(parse_session(&v, Path::new("a"), Some(UUID), 100).is_err());
    }

    #[test]
    fn imports_the_official_launcher_minecraft_token_only() {
        let v = official_fixture();
        let (_, secrets) = parse_session(&v, Path::new("launcher_accounts.json"), None, 0).unwrap();
        assert_eq!(secrets.minecraft_token, "mc-token");
        assert!(secrets.refresh_token.is_empty());
        let saved = serde_json::to_string(&secrets).unwrap();
        assert!(!saved.contains("microsoft-refresh-sentinel"));
        assert!(!saved.contains("microsoft-client-id-sentinel"));

        // A later read must find this account by UUID even if the launcher selection changes.
        let (_, same_account) =
            parse_session(&v, Path::new("launcher_accounts.json"), Some(UUID), 0).unwrap();
        assert_eq!(same_account.minecraft_token, "mc-token");
        let (_, other_account) =
            parse_session(&v, Path::new("launcher_accounts.json"), Some(OTHER_UUID), 0).unwrap();
        assert_eq!(other_account.minecraft_token, "other-mc-token");
    }

    #[test]
    fn official_empty_token_is_unavailable_and_expiry_must_be_rfc3339_and_future() {
        let mut v = official_fixture();
        v["accounts"]["official-primary"]["accessToken"] = json!("");
        let data = parse_session_data(&v, None, 0).unwrap();
        assert_eq!(data.source, Source::Official);
        assert!(data.token.is_none());
        assert!(
            parse_session(&v, Path::new("launcher_accounts.json"), None, 0)
                .unwrap_err()
                .contains("did not expose a reusable Minecraft session")
        );

        v["accounts"]["official-primary"]["accessTokenExpiresAt"] = json!("not-a-date");
        assert!(parse_session_data(&v, None, 0)
            .unwrap_err()
            .contains("session has expired"));
        v["accounts"]["official-primary"]["accessTokenExpiresAt"] = json!("2000-01-01T00:00:00Z");
        assert!(parse_session_data(&v, None, 946_684_800)
            .unwrap_err()
            .contains("session has expired"));
    }

    #[test]
    fn running_and_cached_tokens_are_used_only_for_the_saved_official_account() {
        let mut v = official_fixture();
        v["accounts"]["official-primary"]["accessToken"] = json!("");
        let data = parse_session_data(&v, Some(UUID), 0).unwrap();
        assert_eq!(data.uuid, UUID);
        assert_eq!(
            session_token(
                &data,
                Some("running-token".into()),
                Some(("cached-token", 1000)),
                0,
            )
            .unwrap()
            .0,
            "running-token"
        );
        let (token, expiry) = session_token(&data, None, Some(("cached-token", 1000)), 0).unwrap();
        assert_eq!(token, "cached-token");
        assert_eq!(expiry, 1000);
        assert!(session_token(&data, None, Some(("expired-token", 1)), 0)
            .unwrap_err()
            .contains("session has expired"));

        // The expected UUID still selects the saved profile when the active selection changes.
        let other = parse_session_data(&v, Some(OTHER_UUID), 0).unwrap();
        assert_eq!(other.uuid, OTHER_UUID);
    }

    #[test]
    fn running_game_token_must_match_the_minecraft_process_and_account_uuid() {
        let args = format!(
            "/runtime/bin/java -cp client.jar net.minecraft.client.main.Main --uuid {UUID} --accessToken live-minecraft-token"
        );
        assert_eq!(
            token_from_game_args(&args, UUID).as_deref(),
            Some("live-minecraft-token")
        );
        assert!(token_from_game_args(&args, OTHER_UUID).is_none());
        assert!(token_from_game_args(
            "/runtime/bin/java net.other.Main --uuid 069a79f444e94726a5befca90e38aaf5 --accessToken live-minecraft-token",
            UUID
        )
        .is_none());
        assert!(token_from_game_args(
            "/runtime/bin/java net.minecraft.client.main.Main --uuid 069a79f444e94726a5befca90e38aaf5 --accessToken ${auth_access_token}",
            UUID
        )
        .is_none());
    }

    #[test]
    fn process_command_paths_may_contain_spaces() {
        assert_eq!(
            java_pid_and_command(
                "  4242 /Users/Test Player/Library/Application Support/Minecraft/runtime/bin/java"
            ),
            Some((
                "4242",
                "/Users/Test Player/Library/Application Support/Minecraft/runtime/bin/java"
            ))
        );
        assert!(java_pid_and_command("not-a-pid /usr/bin/java").is_none());
    }

    #[test]
    fn default_import_uses_official_file_before_prism_and_only_falls_back_if_absent() {
        let root = std::env::temp_dir().join(format!(
            "mcre-import-paths-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let official = root.join("official");
        let prism = root.join("prism");
        std::fs::create_dir_all(&official).unwrap();
        std::fs::create_dir_all(&prism).unwrap();
        let official_file = official.join("launcher_accounts.json");
        let prism_file = prism.join("accounts.json");
        std::fs::write(&official_file, b"not valid JSON yet").unwrap();
        std::fs::write(&prism_file, b"Prism session").unwrap();
        let known = vec![
            ("the official launcher", official.clone()),
            ("Prism Launcher", prism.clone()),
        ];

        assert_eq!(default_account_file(&known).unwrap(), official_file);
        std::fs::remove_file(&official_file).unwrap();
        assert_eq!(default_account_file(&known).unwrap(), prism_file);
        std::fs::remove_dir_all(root).unwrap();
    }
}
