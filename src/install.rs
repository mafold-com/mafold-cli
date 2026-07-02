//! `mafold install <tool>` — install a tool the Mafold agent needs.
//!
//! Today the only tool is `claude-code`: detect an existing install, otherwise
//! run Anthropic's official installer (with confirmation), re-verify, and point
//! at the next onboarding step. @mafold:ai teaches this command in-app, so keep
//! its behaviour aligned with the product guide (mafold-api brains/prompts.rs).

use anyhow::{bail, Context, Result};

const CLAUDE_INSTALL_CMD: &str = "curl -fsSL https://claude.ai/install.sh | bash";

pub fn run(tool: &str, yes: bool) -> Result<()> {
    match tool {
        "claude-code" | "claude" | "claudecode" => claude_code(yes),
        other => bail!("unknown tool `{other}` — available: claude-code"),
    }
}

fn claude_code(yes: bool) -> Result<()> {
    if cfg!(windows) {
        bail!("automatic install isn't supported on Windows yet — see https://claude.com/claude-code");
    }
    if crate::harness::on_path("claude") {
        println!("✓ Claude Code already installed ({})", claude_version().unwrap_or_else(|| "version unknown".into()));
        next_steps();
        return Ok(());
    }

    println!("Claude Code (`claude`) not found on PATH.");
    println!("About to run Anthropic's official installer:");
    println!("  {CLAUDE_INSTALL_CMD}");
    if !yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal — re-run with --yes to install non-interactively");
        }
        let answer = crate::prompt("Proceed? [y/N] ");
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("aborted — nothing installed.");
            return Ok(());
        }
    }

    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(CLAUDE_INSTALL_CMD)
        .status()
        .context("failed to run bash")?;
    if !status.success() {
        bail!(
            "installer exited with {status} — try `npm install -g @anthropic-ai/claude-code` \
             or see https://claude.com/claude-code"
        );
    }

    if crate::harness::on_path("claude") {
        println!("✓ Claude Code installed ({})", claude_version().unwrap_or_else(|| "version unknown".into()));
    } else {
        // The native installer's default target (~/.local/bin) may not be on
        // PATH in this shell yet — installed-but-unresolvable is the common
        // case, not an error.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let local = std::path::Path::new(&home).join(".local/bin/claude");
        if local.is_file() {
            println!("✓ installed to {} — open a new terminal (or add ~/.local/bin to PATH) so `claude` resolves", local.display());
        } else {
            bail!("installer finished but `claude` isn't on PATH — open a new terminal and re-check, or see https://claude.com/claude-code");
        }
    }
    next_steps();
    Ok(())
}

fn claude_version() -> Option<String> {
    let out = std::process::Command::new("claude").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn next_steps() {
    println!();
    println!("next steps:");
    println!("  1. run `claude` once in a project folder to sign in to Anthropic");
    println!("  2. in Mafold: pencil icon → New Bot → Runtime: Claude Code");
}
