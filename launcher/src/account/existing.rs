//! Read an existing Prism Launcher Minecraft session without changing its files.
//! Microsoft refresh tokens and application IDs are deliberately never imported.
use super::*;
use std::io::Read;

const EXPIRED: &str = "Your existing Minecraft session has expired. Open Prism Launcher, launch Minecraft once while online, then try again.";

fn read_account_file(path: &Path) -> Result<Value, String> {
    let file = std::fs::File::open(path)
        .map_err(|_| "Could not read the existing launcher's accounts.json.".to_owned())?;
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the existing launcher session.".to_owned())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("The account file is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The existing launcher's account file is not valid JSON.".into())
}

fn parse_session(
    v: &Value,
    path: &Path,
    expected: Option<&str>,
    time: u64,
) -> Result<(String, Secrets), String> {
    let accounts = v
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("Choose a Prism Launcher accounts.json file.")?;
    let selected = accounts
        .iter()
        .find(|a| match expected {
            Some(uuid) => a["profile"]["id"]
                .as_str()
                .is_some_and(|id| id.replace('-', "").eq_ignore_ascii_case(uuid)),
            None => a["active"].as_bool() == Some(true),
        })
        .ok_or("No selected account was found. Select your account in Prism Launcher first.")?;
    let uuid = selected["profile"]["id"]
        .as_str()
        .unwrap_or("")
        .replace('-', "")
        .to_lowercase();
    if uuid.len() != 32 || !uuid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("The existing account has no valid Minecraft profile ID.".into());
    }
    let token = selected["ygg"]["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(EXPIRED)?;
    let expires = selected["ygg"]["exp"].as_u64().ok_or(EXPIRED)?;
    if expires <= time.saturating_add(60) {
        return Err(EXPIRED.into());
    }
    Ok((
        uuid,
        Secrets {
            refresh_token: String::new(),
            imported_from: Some(path.to_owned()),
            minecraft_token: token.to_owned(),
            minecraft_expires_at: expires,
            xuid: None,
        },
    ))
}

pub(super) fn read_session(
    ep: &Endpoints,
    path: &Path,
    expected: Option<&str>,
) -> Result<(Account, Secrets), String> {
    let (uuid, secrets) = parse_session(&read_account_file(path)?, path, expected, now())?;
    // Ask Minecraft, rather than trusting the saved name, UUID or entitlement flag.
    if !owns_game(ep, &secrets.minecraft_token)? {
        return Err("This session does not show ownership of Minecraft: Java Edition.".into());
    }
    let account = profile(ep, &secrets.minecraft_token)?;
    if account.uuid != uuid {
        return Err("The saved session belongs to a different Minecraft account. Sign in again in Prism Launcher.".into());
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
    let paths: Vec<_> = crate::sources::known_folders(crate::sources::Os::current(), &folders)
        .into_iter()
        .filter(|(name, _)| *name == "Prism Launcher")
        .map(|(_, path)| path.join("accounts.json"))
        .filter(|path| path.is_file())
        .collect();
    if paths.len() > 1 {
        return Err("Several Prism Launcher installs were found. Use `horizon-launcher account import PATH` to choose their accounts.json file.".into());
    }
    let path = paths.first().ok_or("No Prism Launcher account file was found. For a custom install, use `horizon-launcher account import PATH`. The official launcher's private account storage is not supported.")?;
    import_from(ep, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    const UUID: &str = "069a79f444e94726a5befca90e38aaf5";
    fn fixture() -> Value {
        json!({"accounts": [{"active": true, "profile": {"id": UUID},
            "msa": {"refresh_token": "must-not-import"}, "msa-client-id": "must-not-import",
            "ygg": {"token": "minecraft-only", "exp": 10000}}]})
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
}
