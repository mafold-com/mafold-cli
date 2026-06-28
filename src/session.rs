//! Human (control-plane) session for mafold-cli. `mafold login` stores the
//! account's `s_…` session token + a stable device id/name in
//! ~/.mafold/session.json. This lets the cli report which coding-agent harnesses
//! are available on THIS machine (→ New-Bot recommendation) and, later,
//! auto-provision bots so the user never pastes a `mafold add --token mb_…`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// `s_…` human session token.
    pub token: String,
    pub username: String,
    /// Stable per-install id (generated once), so the server can tell this
    /// machine apart from the user's other devices.
    pub device_id: String,
    /// Human-readable machine name (hostname).
    pub device_name: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}
fn path() -> PathBuf {
    home().join(".mafold/session.json")
}

pub fn load() -> Option<Session> {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save(s: &Session) -> Result<()> {
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(path(), serde_json::to_string_pretty(s)?).context("write session.json")?;
    Ok(())
}

/// `hostname`, best-effort.
pub fn device_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

/// Reuse the existing device id if present, else mint a stable one. No `uuid`
/// dep — derive 16 hex chars from hostname + pid + nanos (persisted once).
pub fn device_id(existing: Option<&str>) -> String {
    if let Some(id) = existing {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{}-{}", device_name(), std::process::id(), nanos);
    let h = Sha256::digest(seed.as_bytes());
    h[..8].iter().map(|b| format!("{b:02x}")).collect()
}
