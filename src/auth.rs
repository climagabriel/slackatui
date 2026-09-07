//! Credentials for the Web API without anything to copy: the Slack desktop
//! app's session cookie, read from its profile on disk and decrypted the way
//! Chromium stores it, then a token minted by loading the workspace page with
//! that cookie. The recipe is slk's (gammons/slk, itself after gh-slack).
//! Nothing here is ever printed; callers get identity, not secrets.

use std::path::{Path, PathBuf};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha1::Sha1;

#[derive(Clone)]
pub struct Auth {
    pub token: String,
    pub cookie: String,
    /// Where the credentials came from, for the status line.
    pub source: String,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Auth(source={})", self.source)
    }
}

/// The desktop app's profile directory, first that exists: `$SLACK_APP_DIR`,
/// the snap and classic locations under `$HOME`, then every home directory
/// (a root shell on a machine whose desktop session belongs to someone else).
pub fn app_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = std::env::var_os("SLACK_APP_DIR") {
        out.push(PathBuf::from(d));
    }
    if let Some(h) = std::env::var_os("HOME") {
        let h = PathBuf::from(h);
        out.push(h.join("snap/slack/current/.config/Slack"));
        out.push(h.join(".config/Slack"));
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(x).join("Slack"));
    }
    if let Ok(rd) = std::fs::read_dir("/home") {
        let mut homes: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        homes.sort();
        for h in homes {
            out.push(h.join("snap/slack/current/.config/Slack"));
            out.push(h.join(".config/Slack"));
        }
    }
    out.into_iter()
        .filter(|d| d.join("Cookies").is_file())
        .collect()
}

/// The raw `d` cookie row: (plaintext value, encrypted blob).
fn cookie_row(profile: &Path, scratch: &Path) -> Result<(String, Vec<u8>), String> {
    // The app keeps the database open; read a copy, never the live file.
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let tmp = scratch.join(format!("cookies-{}.db", std::process::id()));
    std::fs::copy(profile.join("Cookies"), &tmp).map_err(|e| format!("copy Cookies: {e}"))?;
    let result = (|| {
        let conn =
            rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT value, encrypted_value FROM cookies WHERE host_key = '.slack.com' AND name = 'd'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => "the desktop app is installed but not signed in".to_string(),
            other => other.to_string(),
        })
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Chromium's Linux cookie encryption: AES-128-CBC, key = PBKDF2-SHA1 of a
/// password with salt `saltysalt`, one round, IV of sixteen spaces. `v10`
/// uses the fixed password `peanuts`; `v11` uses one from the keyring.
fn decrypt_cookie(enc: &[u8], password: &[u8]) -> Result<String, String> {
    if enc.len() < 3 {
        return Err("cookie value too short".to_string());
    }
    // One PBKDF2 round is a single HMAC over salt || INT(1).
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(password).map_err(|e| e.to_string())?;
    mac.update(b"saltysalt");
    mac.update(&[0, 0, 0, 1]);
    let key: [u8; 16] = mac.finalize().into_bytes()[..16]
        .try_into()
        .expect("16 bytes");
    let iv = [b' '; 16];
    let plain = Aes128CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&enc[3..])
        .map_err(|_| "cookie did not decrypt (wrong key?)".to_string())?;
    // Chromium 130+ prefixes the value with a SHA-256 of the host key.
    let text = if plain.len() > 32 && !plain[..32].iter().all(|b| (0x20..0x7f).contains(b)) {
        &plain[32..]
    } else {
        &plain[..]
    };
    if text.is_empty() || !text.iter().all(|b| (0x20..0x7f).contains(b)) {
        return Err("decrypted cookie is not printable (wrong key?)".to_string());
    }
    Ok(String::from_utf8_lossy(text).into_owned())
}

fn keyring_passwords() -> Result<Vec<Vec<u8>>, String> {
    // The keyring-backed `v11` case: ask libsecret the way Chromium stores it.
    let out = std::process::Command::new("secret-tool")
        .args(["lookup", "application", "Slack"])
        .output();
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => Ok(vec![o.stdout]),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(
            "secret-tool is missing; install libsecret-tools to read the Slack desktop keyring"
                .to_string(),
        ),
        Err(error) => Err(format!("cannot run secret-tool: {error}")),
        _ => Ok(Vec::new()),
    }
}

/// The `d` cookie of the signed-in desktop app.
pub fn desktop_cookie(profile: &Path, scratch: &Path) -> Result<String, String> {
    let (plain, enc) = cookie_row(profile, scratch)?;
    if !plain.is_empty() {
        return Ok(plain);
    }
    match enc.get(..3) {
        Some(b"v10") => decrypt_cookie(&enc, b"peanuts"),
        Some(b"v11") => {
            let pws = keyring_passwords()?;
            if pws.is_empty() {
                return Err(
                    "cookie is keyring-encrypted (v11) and secret-tool found no Slack entry"
                        .to_string(),
                );
            }
            let mut last = String::new();
            for pw in pws {
                match decrypt_cookie(&enc, &pw) {
                    Ok(v) => return Ok(v),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
        _ => Err("unknown cookie encryption version".to_string()),
    }
}

/// Limit workspace hints to Slack hosts before attaching a session cookie.
fn normalize_workspace_url(name: &str) -> Option<String> {
    let host = name
        .trim()
        .strip_prefix("https://")
        .unwrap_or(name.trim())
        .trim_end_matches('/');
    let label = host.strip_suffix(".slack.com").unwrap_or(host);
    if label.is_empty()
        || label.len() > 63
        || label.starts_with('-')
        || label.ends_with('-')
        || !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    Some(format!("https://{}.slack.com", label.to_ascii_lowercase()))
}

/// Without an archive, use the same workspace selection as slack-workspace-auth.
pub fn selected_workspace_url() -> Option<String> {
    if let Ok(name) = std::env::var("SLACK_WORKSPACE") {
        if !name.trim().is_empty() {
            return normalize_workspace_url(&name);
        }
    }
    let cache = std::env::var_os("SLACKDUMP_CACHE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|value| !value.is_empty())
                .map(|path| PathBuf::from(path).join("slackdump"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/slackdump"))
        })?;
    normalize_workspace_url(&std::fs::read_to_string(cache.join("workspace.txt")).ok()?)
}

/// Mint an `xoxc` token: the workspace page embeds one for the session.
pub fn mint_token(
    agent: &ureq::Agent,
    workspace_url: &str,
    cookie: &str,
) -> Result<String, String> {
    let url = format!("{}/", workspace_url.trim_end_matches('/'));
    let mut resp = agent
        .get(&url)
        .header("Cookie", &format!("d={cookie}"))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let token = body
        .split("\"api_token\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .filter(|t| t.starts_with("xoxc-"))
        .map(str::to_string);
    token.ok_or_else(|| {
        "the workspace page carried no api_token: the desktop session may have expired".to_string()
    })
}

/// The token cache: `~/.config/slack-tui/auth.json`, owner-only.
pub fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("slack-tui").join("auth.json"))
}

pub fn load_cached() -> Option<Auth> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    let cookie = v.get("cookie")?.as_str()?.to_string();
    Some(Auth {
        token,
        cookie,
        source: "cached token".to_string(),
    })
}

pub fn save_cached(auth: &Auth) -> Result<(), String> {
    use std::io::Write;
    let Some(path) = cache_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::json!({ "token": auth.token, "cookie": auth.cookie }).to_string();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// Credentials, in order: the environment, the cached token, the desktop app.
pub fn from_env() -> Option<Auth> {
    let token = std::env::var("SLACK_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    let cookie = std::env::var("SLACK_COOKIE")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(Auth {
        token,
        cookie,
        source: "SLACK_TOKEN and SLACK_COOKIE".to_string(),
    })
}

pub fn from_desktop(
    agent: &ureq::Agent,
    workspace_url: &str,
    scratch: &Path,
) -> Result<Auth, String> {
    let dirs = app_dirs();
    let Some(profile) = dirs.first() else {
        return Err(
            "no Slack desktop profile found (SLACK_APP_DIR, ~/snap/slack, ~/.config/Slack)"
                .to_string(),
        );
    };
    let cookie = desktop_cookie(profile, scratch)?;
    let token = mint_token(agent, workspace_url, &cookie)?;
    Ok(Auth {
        token,
        cookie,
        source: format!("desktop app profile {}", profile.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    #[test]
    fn workspace_selection_accepts_slack_hosts_only() {
        for value in ["myorg", "myorg.slack.com", " https://myorg.slack.com/\n"] {
            assert_eq!(
                normalize_workspace_url(value).as_deref(),
                Some("https://myorg.slack.com")
            );
        }
        for value in [
            "",
            "https://evil.example",
            "myorg.slack.com.evil.example",
            "user@myorg.slack.com",
            "http://myorg.slack.com",
            "myorg.slack.com:443",
            "myorg/path",
            "-myorg",
        ] {
            assert!(normalize_workspace_url(value).is_none(), "accepted {value}");
        }
    }

    fn encrypt(plain: &[u8], password: &[u8]) -> Vec<u8> {
        let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(password).unwrap();
        mac.update(b"saltysalt");
        mac.update(&[0, 0, 0, 1]);
        let key: [u8; 16] = mac.finalize().into_bytes()[..16].try_into().unwrap();
        let iv = [b' '; 16];
        let body = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(plain);
        let mut out = b"v10".to_vec();
        out.extend(body);
        out
    }

    #[test]
    fn v10_round_trip_with_and_without_domain_hash() {
        let enc = encrypt(b"xoxd-secret-value", b"peanuts");
        assert_eq!(
            decrypt_cookie(&enc, b"peanuts").unwrap(),
            "xoxd-secret-value"
        );
        let mut with_hash = vec![0u8; 32];
        with_hash.extend(b"xoxd-secret-value");
        let enc = encrypt(&with_hash, b"peanuts");
        assert_eq!(
            decrypt_cookie(&enc, b"peanuts").unwrap(),
            "xoxd-secret-value"
        );
        assert!(decrypt_cookie(&enc, b"wrong").is_err());
    }
}
