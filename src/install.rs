//! `mafold install <runtime>` — install a coding-agent runtime the Mafold agent
//! drives (claude-code / codex / kimi-code / opencode).
//!
//! Each runtime: detect an existing install, otherwise run its OFFICIAL
//! installer (with confirmation; `--yes` for headless), re-verify, and point at
//! the next onboarding step. `mafold install` with no argument lists the
//! runtimes + their install state. The web New-Bot modal's one-click install
//! rides this too: the supervisor claims a queued install (`claimProvisions`
//! → `installs`) and calls [`run`] with `yes = true`. @mafold:ai teaches this
//! command in-app, so keep behaviour aligned with the product guide
//! (mafold-api brains/prompts.rs) and the api's `INSTALLABLE_RUNTIMES`.

use anyhow::{bail, Context, Result};

/// One installable runtime. Order = the product's canonical display order
/// (claude code / codex / kimi / opencode) — the web install page mirrors it.
struct Tool {
    /// Stable harness id (matches `harness::KNOWN` / the api).
    id: &'static str,
    /// Accepted aliases for the CLI argument.
    aliases: &'static [&'static str],
    /// Human name for prints.
    name: &'static str,
    /// The binary whose presence on PATH means "installed".
    bin: &'static str,
    /// The official installer, run via `bash -c`.
    install_cmd: &'static str,
    /// Manual fallback shown when the installer fails.
    fallback: &'static str,
    /// Post-install auth step (every runtime authenticates on this machine).
    auth_hint: &'static str,
}

const TOOLS: &[Tool] = &[
    Tool {
        id: "claude-code",
        aliases: &["claude-code", "claude", "claudecode"],
        name: "Claude Code",
        bin: "claude",
        install_cmd: "curl -fsSL https://claude.ai/install.sh | bash",
        fallback: "try `npm install -g @anthropic-ai/claude-code` or see https://claude.com/claude-code",
        auth_hint: "run `claude` once in a project folder to sign in to Anthropic",
    },
    Tool {
        id: "codex",
        aliases: &["codex", "codex-cli"],
        name: "Codex",
        bin: "codex",
        install_cmd: "npm install -g @openai/codex",
        fallback: "needs Node/npm on PATH — `brew install node` first, or see https://developers.openai.com/codex/cli",
        auth_hint: "run `codex` once to sign in to OpenAI",
    },
    Tool {
        id: "kimi-code",
        aliases: &["kimi-code", "kimi", "kimi-cli", "kimicode"],
        name: "Kimi Code",
        bin: "kimi",
        install_cmd: "curl -LsSf https://code.kimi.com/install.sh | bash",
        fallback: "see https://moonshotai.github.io/kimi-cli/",
        auth_hint: "run `kimi login` to sign in to Moonshot",
    },
    Tool {
        id: "opencode",
        aliases: &["opencode", "open-code"],
        name: "OpenCode",
        bin: "opencode",
        install_cmd: "curl -fsSL https://opencode.ai/install | bash",
        fallback: "try `npm install -g opencode-ai` or see https://opencode.ai",
        auth_hint: "run `opencode auth login` to connect a provider",
    },
];

fn find(tool: &str) -> Option<&'static Tool> {
    let q = tool.trim().to_lowercase();
    TOOLS.iter().find(|t| t.aliases.contains(&q.as_str()))
}

pub fn run(tool: &str, yes: bool) -> Result<()> {
    if tool.trim().is_empty() || tool == "list" {
        return list();
    }
    let Some(t) = find(tool) else {
        bail!(
            "unknown runtime `{tool}` — available: {}",
            TOOLS.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
        );
    };
    install(t, yes)
}

/// `mafold install` with no argument: the four runtimes + their install state.
fn list() -> Result<()> {
    println!("runtimes:");
    for t in TOOLS {
        let state = if crate::harness::on_path(t.bin) {
            format!(
                "✓ installed{}",
                version_of(t.bin)
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default()
            )
        } else {
            format!("· not installed — `mafold install {}`", t.id)
        };
        println!("  {:<12} {}", t.id, state);
    }
    Ok(())
}

fn install(t: &Tool, yes: bool) -> Result<()> {
    if cfg!(windows) {
        bail!(
            "automatic install isn't supported on Windows yet — {}",
            t.fallback
        );
    }
    if crate::harness::on_path(t.bin) {
        println!(
            "✓ {} already installed ({})",
            t.name,
            version_of(t.bin).unwrap_or_else(|| "version unknown".into())
        );
        next_steps(t);
        return Ok(());
    }

    println!("{} (`{}`) not found on PATH.", t.name, t.bin);
    println!("About to run the official installer:");
    println!("  {}", t.install_cmd);
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
        .arg(t.install_cmd)
        .status()
        .context("failed to run bash")?;
    if !status.success() {
        bail!("installer exited with {status} — {}", t.fallback);
    }

    if crate::harness::on_path(t.bin) {
        println!(
            "✓ {} installed ({})",
            t.name,
            version_of(t.bin).unwrap_or_else(|| "version unknown".into())
        );
    } else {
        // Installers default to ~/.local/bin (or npm's prefix), which may not be
        // on PATH in this shell yet — installed-but-unresolvable is the common
        // case, not an error.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let local = std::path::Path::new(&home).join(".local/bin").join(t.bin);
        if local.is_file() {
            println!("✓ installed to {} — open a new terminal (or add ~/.local/bin to PATH) so `{}` resolves", local.display(), t.bin);
        } else {
            bail!("installer finished but `{}` isn't on PATH — open a new terminal and re-check, or: {}", t.bin, t.fallback);
        }
    }
    next_steps(t);
    Ok(())
}

fn version_of(bin: &str) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn next_steps(t: &Tool) {
    println!();
    println!("next steps:");
    println!("  1. {}", t.auth_hint);
    println!(
        "  2. in Mafold: pencil icon → New Bot → Runtime: {}",
        t.name
    );
}
