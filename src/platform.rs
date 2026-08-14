//! Cross-platform process + file-lock shims.
//!
//! The daemon/supervisor logic is identical on every OS — only the primitives
//! differ: detaching a child from the controlling terminal/console, checking a
//! pid's liveness, terminating it, reaping zombies, and holding a cross-process
//! file lock. Each is implemented once per OS here so the callers stay portable.
//!
//! Unix keeps its exact previous behavior (setsid + `libc::kill`/`waitpid` +
//! `flock`); Windows gets the native equivalents (detached creation flags +
//! `OpenProcess`/`TerminateProcess` + `LockFileEx`).

use std::process::Command;

// ───────────────────────────── Unix ─────────────────────────────
#[cfg(unix)]
mod imp {
    use super::Command;
    use std::os::unix::process::CommandExt;

    /// Is `pid` a live process? `kill(pid, 0)` probes without signalling.
    pub fn pid_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Ask `pid` to stop (SIGTERM — the process can clean up).
    pub fn terminate(pid: u32) {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }

    // NOTE deliberately NO terminate_group here: group-killing a daemon also
    // took down the agent's legitimate cross-turn background tasks (they share
    // the pgroup) — the 2026-07-19 regression. In-flight claude children are
    // killed precisely by the daemon's own shutdown handler instead
    // (harness::live_children).

    /// Configure `cmd` to spawn in a new session, detached from the controlling
    /// terminal, so it survives the launching shell closing.
    pub fn configure_detached(cmd: &mut Command) {
        // SAFETY: setsid() is async-signal-safe and the closure only calls it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    /// Reap any exited children so a zombie pid doesn't fool the liveness check
    /// (a zombie still answers `kill(pid, 0)`).
    pub fn reap_children() {
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if r <= 0 {
                break; // 0 = children exist but none exited; -1 = no children
            }
        }
    }
}

// ──────────────────────────── Windows ───────────────────────────
#[cfg(windows)]
mod imp {
    use super::Command;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess};

    // Process-creation flags (winbase.h). DETACHED_PROCESS gives the child no
    // console → it survives the parent console closing; CREATE_NEW_PROCESS_GROUP
    // stops a parent-console Ctrl+C from propagating into it.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // OpenProcess access rights (processthreadsapi.h / winnt.h).
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    /// Is `pid` a live process? If we can open a handle to it, it exists; once a
    /// process exits and its handles close, `OpenProcess` fails (Windows has no
    /// zombies, so "openable" ≈ "alive" for our own daemons).
    pub fn pid_alive(pid: u32) -> bool {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }

    /// Stop `pid` (TerminateProcess — there is no graceful SIGTERM analogue we
    /// can rely on for a console-less child, so this is a forceful stop).
    pub fn terminate(pid: u32) {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    /// Configure `cmd` to spawn detached from the console so it survives the
    /// launching shell closing.
    pub fn configure_detached(cmd: &mut Command) {
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    /// No zombies on Windows — nothing to reap.
    pub fn reap_children() {}
}

pub use imp::*;

/// Hide the console window this process owns (Windows). A supervisor launched
/// by the Task Scheduler logon task is a console binary, so Windows hands it a
/// fresh visible console at sign-in; it runs headless, so `supervise --hidden`
/// hides that window right after startup. `GetConsoleWindow` returns NULL when
/// there is no console (already detached) — nothing to hide then. No-op on Unix
/// (launchd/systemd services never get a terminal).
pub fn hide_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        unsafe {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

/// Stop a spawned console child (e.g. `claude`) from popping up its own console
/// window on Windows. The agent runs detached (no console), so a console child
/// would otherwise be handed a fresh visible window until it produces output.
/// No-op on Unix.
pub fn no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Same, for `std::process::Command` — helper subprocesses (schtasks, the
/// update smoke test's `--version` run) would each flash a visible console
/// window when the parent runs console-less (a detached supervisor/agent).
/// Their output is read over pipes, so hiding the window changes nothing else.
/// No-op on Unix.
pub fn no_window_std(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Best-effort "open this URL in the default browser" (`mafold login`'s device
/// flow, à la `gh auth login`). Headless boxes and weird shells just fail the
/// spawn — the caller always prints the URL too, so nothing depends on this.
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        // `start` is a cmd.exe builtin; the empty "" is its window-title slot,
        // which otherwise eats a quoted URL.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        no_window_std(&mut c);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ────────────────────── cross-process file lock ──────────────────────
// `update.rs` holds an exclusive lock on `~/.mafold/update.lock` while it swaps
// the binary. Unix does this inline with `flock`; Windows needs `LockFileEx`.

/// Block until an exclusive lock is held on `file` (released when the file's
/// handle closes). Windows-only — Unix uses `flock` directly in `update.rs`.
#[cfg(windows)]
pub fn lock_file_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ──────────────────────────── re-exec ────────────────────────────
// Self-update restarts into the freshly-swapped binary. Unix replaces the
// process image via `exec()` (keeps the same pid). Windows has no `exec`, so we
// spawn a fresh copy with the same args/env and exit.

/// Replace the current process with a fresh `mafold` of the same args. Never
/// returns on success.
pub fn reexec() -> std::io::Error {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("mafold"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Command::new(exe).args(std::env::args_os().skip(1)).exec()
    }
    #[cfg(windows)]
    {
        match Command::new(exe).args(std::env::args_os().skip(1)).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}
