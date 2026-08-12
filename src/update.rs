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

const REPO: &str = "mafold-lab/mafold-cli";

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
    let mut it = v.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
fn is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}
fn is_same_version(latest: &str, current: &str) -> bool {
    parse_semver(latest) == parse_semver(current)
}

/// Should we install `r`?
///
/// A HIGHER version, obviously. But also the SAME version when our binary is
/// demonstrably not the build that release published — compared by checksum,
/// the only thing that actually identifies a build.
///
/// THE HOLE THIS CLOSES. `Cargo.toml` is bumped on main BEFORE the tag is cut,
/// so every build of main in that window reports the release's version without
/// containing it. Install one (a hand-built binary, a colleague's copy) and the
/// version compare says "already on 0.9.81" forever: the real 0.9.81 can never
/// arrive, because it is not *newer*. Observed 2026-08-02 — a binary built 2h
/// before the fix it was supposed to carry sat on a machine for a day, and
/// `/usage` rendered "Unsupported card: stats" the whole time with the daemon
/// cheerfully reporting itself up to date.
///
/// Conservative on purpose: with no published checksum to compare against we
/// never trigger a same-version install, so an unverifiable release can't put
/// the updater into a download loop.
fn wants_update(r: &Release, our_sha: Option<&str>) -> bool {
    if is_newer(&r.version, current_version()) {
        return true;
    }
    if !is_same_version(&r.version, current_version()) {
        return false; // the "latest" release is older than us — leave us alone
    }
    match (r.sha256.as_deref(), our_sha) {
        (Some(want), Some(got)) => !got.eq_ignore_ascii_case(want),
        _ => false,
    }
}

impl Release {
    /// How to announce applying this release. A same-version install is a
    /// REPAIR, not an upgrade — and it needs to say so out loud, because it is
    /// the one case where the updater replaces a binary the user may have put
    /// there deliberately. ("update v0.9.81 available" while already running
    /// 0.9.81 reads like a bug, and hides why the swap happened.)
    pub fn action_line(&self) -> String {
        if is_same_version(&self.version, current_version()) {
            format!(
                "↻ v{} REINSTALL — this binary is not the build v{} published (checksum differs). \
                 Run the supervisor with --no-auto-update to keep a local build",
                self.version, self.version,
            )
        } else {
            format!("↻ update v{} available", self.version)
        }
    }
}

async fn latest(http: &reqwest::Client) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let v: serde_json::Value = http
        .get(&url)
        .header("User-Agent", "mafold-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = v["tag_name"].as_str().context("no tag_name")?.to_string();
    let target = target_triple().context("unsupported platform for self-update")?;
    let asset_name = format!("mafold-{target}");
    let dl = v["assets"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|x| x["name"].as_str() == Some(&asset_name))
                .and_then(|x| x["browser_download_url"].as_str())
        })
        .with_context(|| format!("release has no asset {asset_name}"))?
        .to_string();
    let sha256 = sha256_for(http, &v, &asset_name).await;
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        url: dl,
        sha256,
    })
}

/// The expected SHA256 for `asset_name`, from its sibling `<asset>.sha256` asset
/// (published by the release workflow). `None` if the asset is absent or the
/// fetch fails → `apply` then ABORTS (an unverifiable binary is never swapped in).
async fn sha256_for(
    http: &reqwest::Client,
    release: &serde_json::Value,
    asset_name: &str,
) -> Option<String> {
    let want = format!("{asset_name}.sha256");
    let url = release["assets"]
        .as_array()?
        .iter()
        .find(|x| x["name"].as_str() == Some(want.as_str()))
        .and_then(|x| x["browser_download_url"].as_str())?;
    let text = http
        .get(url)
        .header("User-Agent", "mafold-cli")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
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
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&p)?;
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

fn stamp_path() -> PathBuf {
    mafold_dir().join("installed-version")
}
fn read_stamp() -> Option<String> {
    std::fs::read_to_string(stamp_path())
        .ok()
        .map(|s| s.trim().to_string())
}

// ── supervisor update nudge ──
fn nudge_path() -> PathBuf {
    mafold_dir().join("update-nudge")
}
/// Ask the supervisor to run an update check NOW. A `--no-auto-update` agent
/// child writes this on `events.cliUpdate` (supervised mode) so the SUPERVISOR —
/// which owns updates and respawns children — applies it, instead of the child
/// self-re-execing out from under the supervisor.
pub fn request_nudge() {
    let p = nudge_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
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

/// Checksum of a binary on disk — what a build IS, as opposed to what its
/// `--version` claims to be. `None` when it can't be read (then every caller
/// falls back to comparing versions, i.e. the old behaviour).
///
/// Cost is a ~10 MB read + hash, paid on the update poll (every 10 minutes) and
/// on a nudge — not on any hot path.
fn sha256_of_file(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| sha256_hex(&b))
}

/// Quick "does it run?" check on the downloaded binary before swapping it in.
fn smoke_test(path: &Path) -> bool {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--version");
    crate::platform::no_window_std(&mut cmd); // a hidden supervisor must not flash a console
    cmd.output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Does the on-disk binary actually report `version`? Guards the stamp early-exit
/// in `apply`: the stamp is machine-global (`~/.mafold/installed-version`), but a
/// machine can end up with several mafold copies (a manual install over an
/// updated one, a second checkout's build) — then the stamp says "vX installed"
/// while OUR file is still older. Trusting it made apply() return Ok without
/// doing anything, and the caller's reexec respawned the same old binary forever.
fn binary_is(bin: &Path, version: &str) -> bool {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--version");
    crate::platform::no_window_std(&mut cmd); // a hidden supervisor must not flash a console
    cmd.output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(version))
        .unwrap_or(false)
}

// ── auto-update failure cooldown ──
// github may be unreachable for a long stretch (offline, or blocked networks —
// real case: release-asset downloads reset while the API is fine). Without a
// cooldown the agent/supervisor re-download-and-fail on every tick/cliUpdate
// nudge, spamming the log and wasting bandwidth forever. Remember the version
// that just failed and skip re-attempts for a while; a manual `mafold update`
// is not throttled (it never consults this).
static LAST_FAILED: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);
const FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3600);

/// Record that auto-updating to `version` just failed (starts the cooldown).
pub fn mark_failed(version: &str) {
    *LAST_FAILED.lock().unwrap() = Some((version.to_string(), std::time::Instant::now()));
}

/// Should the auto-updater skip `version` because it failed recently?
pub fn recently_failed(version: &str) -> bool {
    LAST_FAILED
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(v, at)| v == version && at.elapsed() < FAILURE_COOLDOWN)
}

/// Is a newer release available? Returns it (no download). No-ops on platforms
/// the release workflow doesn't build a binary for (e.g. Windows) so the
/// auto-update tick stays quiet instead of erroring every cycle.
pub async fn check(http: &reqwest::Client) -> Result<Option<Release>> {
    if target_triple().is_none() {
        return Ok(None);
    }
    let r = latest(http).await?;
    // Hashed ONLY when the versions already match — the common "we're current"
    // tick still costs nothing beyond the API call it was already making.
    let our_sha = is_same_version(&r.version, current_version())
        .then(|| binary_path().ok().as_deref().and_then(sha256_of_file))
        .flatten();
    Ok(wants_update(&r, our_sha.as_deref()).then_some(r))
}

/// Safely download + verify + swap in the binary. Coordinated across processes
/// via flock + a version stamp (a concurrent updater either wins or no-ops).
pub async fn apply(
    http: &reqwest::Client,
    url: &str,
    version: &str,
    sha256: Option<&str>,
) -> Result<()> {
    let _lock = acquire_lock()?;
    let bin = binary_path()?;
    // Someone else already installed this version while we waited on the lock —
    // but only trust the stamp if OUR binary really is that version (the stamp is
    // machine-global; a stale one from another copy/install must not short-circuit
    // us into an eternal "apply Ok → reexec the same old exe" spin).
    //
    // The CHECKSUM clause is what makes a same-version repair possible at all:
    // without it, a binary that merely CLAIMS the right version satisfies both
    // tests above and this returns Ok having done nothing — the other half of
    // the trap `wants_update` exists to escape.
    let already_the_published_build = match (sha256, sha256_of_file(&bin)) {
        (Some(want), Some(got)) => got.eq_ignore_ascii_case(want),
        // No checksum to compare: fall back to the version-only rule, exactly
        // as before.
        _ => true,
    };
    if read_stamp().as_deref() == Some(version) && binary_is(&bin, version) && already_the_published_build {
        return Ok(());
    }
    // Fail CLOSED: the binary we're about to swap in runs
    // `--dangerously-skip-permissions`, so a download we can't verify is never
    // applied. A missing/unfetchable `<asset>.sha256` aborts the update.
    let want = sha256.context(
        "release is missing its .sha256 checksum asset — refusing to update an unverifiable binary",
    )?;
    let bytes = http
        .get(url)
        .header("User-Agent", "mafold-cli")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
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
        anyhow::bail!(
            "no backup to roll back to (looked for {})",
            backup.display()
        );
    }
    install_running_binary(&backup, &bin, false)?;
    let _ = std::fs::remove_file(stamp_path());
    println!(
        "✓ rolled back to the previous binary ({})",
        backup.display()
    );
    Ok(())
}

/// Check + update if newer. Returns the new version if updated. (`mafold update`)
pub async fn update_to_latest(http: &reqwest::Client) -> Result<Option<String>> {
    match check(http).await? {
        Some(r) => {
            apply(http, &r.url, &r.version, r.sha256.as_deref()).await?;
            Ok(Some(r.version))
        }
        None => Ok(None),
    }
}

/// Replace the current process with the (freshly updated) binary, keeping the
/// same args + env. Never returns on success. Unix `exec`s in place (same pid);
/// Windows respawns and exits — see `crate::platform::reexec`.
pub fn reexec() -> std::io::Error {
    crate::platform::reexec()
}

/// `reexec`, but LOUD on failure. After `apply()` the binary on disk is already
/// `new_version`; if the re-exec fails THIS process silently keeps running the
/// old code — the "disk says new, behavior says old" stale-daemon trap (easiest
/// to hit on Windows, where a virus scanner can hold the fresh exe just long
/// enough for the spawn to fail). Retry once after a beat, then leave an
/// unmissable log line instead of the old `let _ =` swallow.
pub fn reexec_or_warn(new_version: &str) {
    let e = reexec(); // only returns on failure
    std::thread::sleep(std::time::Duration::from_millis(750));
    let e2 = reexec();
    eprintln!(
        "⚠️  re-exec into v{new_version} FAILED (first: {e}; retry: {e2}) — STILL RUNNING v{} \
         while the binary on disk is v{new_version}. Restart me: `mafold down && mafold up`.",
        current_version()
    );
}

#[cfg(test)]
mod tests {
    use super::{current_version, is_newer, wants_update, Release};

    fn rel(version: &str, sha: Option<&str>) -> Release {
        Release {
            version: version.into(),
            url: "https://example.invalid/mafold".into(),
            sha256: sha.map(Into::into),
        }
    }

    /// Bump the patch of whatever version this build actually is, so the tests
    /// don't need editing every release.
    fn newer_than_current() -> String {
        let mut p = current_version().split('.').map(|s| s.to_string()).collect::<Vec<_>>();
        let last = p.len() - 1;
        p[last] = (p[last].parse::<u64>().unwrap_or(0) + 1).to_string();
        p.join(".")
    }

    #[test]
    fn a_newer_release_always_wins() {
        // …and needs no checksum to do it — the version compare is enough.
        assert!(wants_update(&rel(&newer_than_current(), None), Some("aaaa")));
        assert!(wants_update(&rel(&newer_than_current(), Some("bbbb")), Some("aaaa")));
        assert!(is_newer(&newer_than_current(), current_version()));
    }

    /// THE HOLE THIS CLOSES. A binary built from main after the version bump but
    /// before the tag reports the release's version WITHOUT containing it. The
    /// version compare calls that "up to date" forever, so the real release can
    /// never land — the checksum is the only thing that tells them apart.
    #[test]
    fn a_same_version_binary_that_isnt_the_published_build_is_replaced() {
        let r = rel(current_version(), Some("cafebabe"));
        assert!(wants_update(&r, Some("deadbeef")));
    }

    /// …and once it IS the published build, the very next check must stop. This
    /// is what keeps the repair from becoming a 10-minute re-download loop.
    #[test]
    fn the_repair_settles_after_one_install() {
        let r = rel(current_version(), Some("cafebabe"));
        assert!(!wants_update(&r, Some("cafebabe")));
        assert!(!wants_update(&r, Some("CAFEBABE")), "hex case must not matter");
    }

    /// Fail SAFE, not eager: with nothing to compare we keep the old
    /// version-only behaviour rather than re-downloading on every tick.
    #[test]
    fn no_checksum_to_compare_means_no_same_version_install() {
        assert!(!wants_update(&rel(current_version(), None), Some("deadbeef")));
        assert!(!wants_update(&rel(current_version(), Some("cafebabe")), None));
        assert!(!wants_update(&rel(current_version(), None), None));
    }

    /// A "latest" release OLDER than us is never installed, checksum or not —
    /// otherwise a mis-tagged release could walk every daemon backwards.
    #[test]
    fn an_older_release_is_never_installed() {
        assert!(!wants_update(&rel("0.0.1", Some("cafebabe")), Some("deadbeef")));
        assert!(!wants_update(&rel("0.0.1", None), None));
    }

    /// The repair says so in plain words: it is the one case where the updater
    /// overwrites a binary someone may have put there on purpose.
    #[test]
    fn a_same_version_install_announces_itself_as_a_repair() {
        let repair = rel(current_version(), Some("cafebabe")).action_line();
        assert!(repair.contains("REINSTALL"), "{repair}");
        assert!(repair.contains("--no-auto-update"), "must name the escape hatch: {repair}");
        assert!(rel(&newer_than_current(), None).action_line().contains("update v"));
    }
}
