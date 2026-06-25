//! Self-update — fetch the latest GitHub release and replace the binary safely.
//!
//! Safety (matters now that one machine runs many daemons sharing one binary):
//! - a cross-process **flock** (`~/.mafold/update.lock`) so only one updater
//!   touches the binary at a time;
//! - a **version stamp** so a second updater skips a download already done;
//! - **MANDATORY SHA256** verification against the release's `<asset>.sha256` —
//!   the self-replacing binary runs `--dangerously-skip-permissions`, so an
//!   unverifiable download (missing/unfetchable checksum) ABORTS the update;
//! - a **pre-swap smoke test** (`<tmp> --version`) so a corrupt/wrong-arch binary
//!   never gets swapped in;
//! - a **backup** (`mafold.old`) for `mafold rollback`.
//!
//! Used by the supervisor (which owns updates for its daemons) and by a
//! standalone `mafold agent` (self-update, unless `--no-auto-update`).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const REPO: &str = "mafold-com/mafold-cli";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A release available for this platform.
pub struct Release {
    pub version: String, // without the leading `v`
    pub url: String,
    pub sha256: Option<String>,
}

fn mafold_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".mafold")
}

/// This build's release-asset target triple (matches the release workflow).
fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// Whether self-update can run here — i.e. the release workflow builds a binary
/// for this platform. False on e.g. linux-arm64, so `mafold update` can say
/// "no build for your platform" instead of the misleading "already up to date".
pub fn platform_supported() -> bool {
    target_triple().is_some()
}

fn parse_semver(v: &str) -> (u64, u64, u64) {
    let v = v.trim().trim_start_matches('v');
    let mut it = v.split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}
fn is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

async fn latest(http: &reqwest::Client) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let v: serde_json::Value = http
        .get(&url)
        .header("User-Agent", "mafold-cli")
        .header("Accept", "application/vnd.github+json")
        .send().await?
        .error_for_status()?
        .json().await?;
    let tag = v["tag_name"].as_str().context("no tag_name")?.to_string();
    let target = target_triple().context("unsupported platform for self-update")?;
    let asset_name = format!("mafold-{target}");
    let dl = v["assets"].as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(&asset_name)).and_then(|x| x["browser_download_url"].as_str()))
        .with_context(|| format!("release has no asset {asset_name}"))?.to_string();
    let sha256 = sha256_for(http, &v, &asset_name).await;
    Ok(Release { version: tag.trim_start_matches('v').to_string(), url: dl, sha256 })
}

/// The expected SHA256 for `asset_name`, from its sibling `<asset>.sha256` asset
/// (published by the release workflow). `None` if the asset is absent or the
/// fetch fails → `apply` then ABORTS (an unverifiable binary is never swapped in).
async fn sha256_for(http: &reqwest::Client, release: &serde_json::Value, asset_name: &str) -> Option<String> {
    let want = format!("{asset_name}.sha256");
    let url = release["assets"].as_array()?
        .iter().find(|x| x["name"].as_str() == Some(want.as_str()))
        .and_then(|x| x["browser_download_url"].as_str())?;
    let text = http.get(url).header("User-Agent", "mafold-cli").send().await.ok()?.text().await.ok()?;
    text.split_whitespace().next().map(|s| s.to_string())
}

/// The real binary path (resolve a PATH symlink to the file we replace).
fn binary_path() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

// ── cross-process update lock (flock) ──
/// Holds the lock file open; the flock is released when this drops (fd closes).
struct Lock(#[allow(dead_code)] std::fs::File);
fn acquire_lock() -> Result<Lock> {
    let p = mafold_dir().join("update.lock");
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&p)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            anyhow::bail!("couldn't acquire the update lock");
        }
    }
    #[cfg(windows)]
    crate::platform::lock_file_exclusive(&f).context("couldn't acquire the update lock")?;
    Ok(Lock(f)) // released when the File (handle) drops
}

fn stamp_path() -> PathBuf { mafold_dir().join("installed-version") }
fn read_stamp() -> Option<String> { std::fs::read_to_string(stamp_path()).ok().map(|s| s.trim().to_string()) }

// ── supervisor update nudge ──
fn nudge_path() -> PathBuf { mafold_dir().join("update-nudge") }
/// Ask the supervisor to run an update check NOW. A `--no-auto-update` agent
/// child writes this on `events.cliUpdate` (supervised mode) so the SUPERVISOR —
/// which owns updates and respawns children — applies it, instead of the child
/// self-re-execing out from under the supervisor.
pub fn request_nudge() {
    let p = nudge_path();
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(p, b"");
}
/// Consume a pending update nudge (true if one was set). The supervisor checks
/// this each loop tick for an immediate (non-poll) update.
pub fn take_nudge() -> bool {
    let p = nudge_path();
    if p.exists() {
        let _ = std::fs::remove_file(&p);
        true
    } else {
        false
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Quick "does it run?" check on the downloaded binary before swapping it in.
fn smoke_test(path: &Path) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Is a newer release available? Returns it (no download). No-ops on platforms
/// the release workflow doesn't build a binary for (e.g. Windows) so the
/// auto-update tick stays quiet instead of erroring every cycle.
pub async fn check(http: &reqwest::Client) -> Result<Option<Release>> {
    if target_triple().is_none() {
        return Ok(None);
    }
    let r = latest(http).await?;
    Ok(is_newer(&r.version, current_version()).then_some(r))
}

/// Safely download + verify + swap in the binary. Coordinated across processes
/// via flock + a version stamp (a concurrent updater either wins or no-ops).
pub async fn apply(http: &reqwest::Client, url: &str, version: &str, sha256: Option<&str>) -> Result<()> {
    let _lock = acquire_lock()?;
    // Someone else already installed this version while we waited on the lock.
    if read_stamp().as_deref() == Some(version) {
        return Ok(());
    }
    let bin = binary_path()?;
    // Fail CLOSED: the binary we're about to swap in runs
    // `--dangerously-skip-permissions`, so a download we can't verify is never
    // applied. A missing/unfetchable `<asset>.sha256` aborts the update.
    let want = sha256.context(
        "release is missing its .sha256 checksum asset — refusing to update an unverifiable binary",
    )?;
    let bytes = http.get(url).header("User-Agent", "mafold-cli").send().await?.error_for_status()?.bytes().await?;
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(want) {
        anyhow::bail!("checksum mismatch (want {want}, got {got}) — refusing to update");
    }
    // Unique temp per process so concurrent updaters never clobber each other.
    // Keep the binary's extension (Windows needs `.exe` to run the smoke test).
    let tmp = match bin.extension().and_then(|e| e.to_str()) {
        Some(ext) => bin.with_file_name(format!("mafold.new.{}.{ext}", std::process::id())),
        None => bin.with_file_name(format!("mafold.new.{}", std::process::id())),
    };
    std::fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    if !smoke_test(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("downloaded binary failed its `--version` smoke test — refusing to update");
    }
    // Back up the current binary for `mafold rollback` (reading/copying a running
    // exe is fine on every OS), then swap the new one in.
    let _ = std::fs::copy(&bin, bin.with_file_name("mafold.old"));
    install_running_binary(&tmp, &bin, true)?;
    let _ = std::fs::write(stamp_path(), version);
    Ok(())
}

/// Install `src` as `bin`, where `bin` may be the CURRENTLY RUNNING executable.
/// Unix overwrites in place (rename/copy straight over it). Windows can't
/// overwrite a running `.exe`, but it CAN rename the live file aside (the running
/// process keeps executing the moved image), so we move it out of the way first,
/// then put the new binary at the canonical path. `mv` renames `src` in (consumes
/// it, for apply); `!mv` copies (keeps it, for rollback's backup).
fn install_running_binary(src: &Path, bin: &Path, mv: bool) -> Result<()> {
    #[cfg(windows)]
    {
        let aside = bin.with_file_name(format!("mafold.prev.{}.exe", std::process::id()));
        let _ = std::fs::remove_file(&aside);
        if bin.exists() {
            std::fs::rename(bin, &aside).context("failed to move the running binary aside")?;
        }
        let placed = if mv {
            std::fs::rename(src, bin)
        } else {
            std::fs::copy(src, bin).map(|_| ())
        };
        if let Err(e) = placed {
            let _ = std::fs::rename(&aside, bin); // undo so we never lose the binary
            return Err(e).context("failed to place the new binary");
        }
        // Best-effort: the old image is still memory-mapped by the live process, so
        // this may fail; it then lingers harmlessly until the next launch.
        let _ = std::fs::remove_file(&aside);
        Ok(())
    }
    #[cfg(unix)]
    {
        if mv {
            std::fs::rename(src, bin).context("failed to replace the binary")?;
        } else {
            std::fs::copy(src, bin).context("failed to restore the previous binary")?;
        }
        Ok(())
    }
}

/// Restore the previous binary (`mafold.old`) — `mafold rollback`.
pub fn rollback() -> Result<()> {
    let bin = binary_path()?;
    let backup = bin.with_file_name("mafold.old");
    if !backup.exists() {
        anyhow::bail!("no backup to roll back to (looked for {})", backup.display());
    }
    install_running_binary(&backup, &bin, false)?;
    let _ = std::fs::remove_file(stamp_path());
    println!("✓ rolled back to the previous binary ({})", backup.display());
    Ok(())
}

/// Check + update if newer. Returns the new version if updated. (`mafold update`)
pub async fn update_to_latest(http: &reqwest::Client) -> Result<Option<String>> {
    match check(http).await? {
        Some(r) => { apply(http, &r.url, &r.version, r.sha256.as_deref()).await?; Ok(Some(r.version)) }
        None => Ok(None),
    }
}

/// Replace the current process with the (freshly updated) binary, keeping the
/// same args + env. Never returns on success. Unix `exec`s in place (same pid);
/// Windows respawns and exits — see `crate::platform::reexec`.
pub fn reexec() -> std::io::Error {
    crate::platform::reexec()
}
