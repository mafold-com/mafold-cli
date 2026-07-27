//! Draft polish — the model side of the composer's ✦.
//!
//! This is deliberately NOT a turn. It does not touch the agent session, the
//! chat history, or the bot's turn machinery: it shells out to a coding-agent
//! CLI once, headlessly, and hands back a rewritten string. The bot the user is
//! talking to is never woken.
//!
//! Everything here was measured against the real CLIs on 2026-07-26 (Windows,
//! claude 2.1.220 / codex 0.145.0-alpha.18 / kimi 1.49.0). The numbers matter,
//! because they are nothing like what a chat completion costs:
//!
//! | channel      | short  | medium | long (1k chars) |
//! |--------------|--------|--------|-----------------|
//! | claude-code  |  7.3s  |  8.0s  |  9.3s           |
//! | codex        | 12.6s  |  5.7s  |  6.7s           |
//! | kimi-code    |  7.5s  |  7.7s  | 11.5s           |
//!
//! A one-shot agent CLI spends most of that on process + session startup, so
//! latency barely tracks input length. Anything in the UI that assumes a
//! sub-second polish is wrong.

use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Instant;
use tokio::process::Command;

/// Hard ceiling for one polish. Long drafts measured ~12s, so this leaves
/// generous headroom while still guaranteeing the caller gets an answer.
const POLISH_TIMEOUT_SECS: u64 = 60;
/// Probing is just `--version` per CLI; if that hangs, the CLI is broken.
const PROBE_TIMEOUT_SECS: u64 = 6;

/// A local CLI login that can rewrite a draft.
struct Spec {
    id: &'static str,
    label: &'static str,
    bin: &'static str,
    /// How the user pays. `metered` = per-token money; `subscription` = a quota
    /// window that real work also draws on.
    kind: &'static str,
    /// Measured p50 of a real one-shot call. Used as the seed for ranking until
    /// the daemon has observed enough live calls of its own.
    p50_ms: u32,
}

const SPECS: &[Spec] = &[
    Spec { id: "kimi-code",   label: "Kimi Code",   bin: "kimi",   kind: "metered",      p50_ms: 7500 },
    Spec { id: "claude-code", label: "Claude Code", bin: "claude", kind: "subscription", p50_ms: 7300 },
    Spec { id: "codex",       label: "Codex",       bin: "codex",  kind: "subscription", p50_ms: 8300 },
];

/// The polish instruction. L1 is a copy-edit and is explicitly forbidden from
/// changing meaning; L2 is allowed to restructure, because that is the whole
/// point of asking for it twice.
fn prompt_for(level: u8, text: &str) -> String {
    let rules = if level >= 2 {
        "你是一个中文写作结构化助手。把下面的内容重新组织成清晰的结构：可以拆成小标题、编号或要点，\
         可以合并重复、调整顺序、改写句子。保留全部事实与意图，不要新增信息，不要评论。"
    } else {
        "你是一个中文文本润色器。修正错别字与标点，理顺语序，让表达更清楚。\
         不要改变原意，不要新增内容，不要删减信息，保持原有段落结构。"
    };
    format!(
        "{rules}\n\n只输出结果正文。不要解释，不要加引号，不要加标题，不要用项目符号包裹整体输出。\n\n待处理：\n{text}"
    )
}

/// Strip the decoration a CLI wraps around its answer. Kimi's print UI prefixes
/// a bullet and hard-wraps at the terminal width; the others are clean. Any
/// leading/trailing fence or quote the model added anyway also goes.
fn tidy(raw: &str, id: &str) -> String {
    let mut s = raw.replace('\r', "");
    if id == "kimi-code" {
        s = unwrap_kimi(&s);
    }
    let s = s.trim();
    let s = s
        .strip_prefix("```")
        .and_then(|r| r.split_once('\n'))
        .map(|(_, rest)| rest)
        .unwrap_or(s);
    s.trim().trim_end_matches("```").trim().to_string()
}

/// Kimi's print UI is a terminal renderer, not a pipe: it bullets the answer,
/// hard-wraps it to the terminal width with a two-space hanging indent, and —
/// measured repeatedly — likes to narrate first ("Just polish the text.").
/// Undo all three, or the composer receives an English preamble and a draft
/// full of line breaks that were never in the text.
fn unwrap_kimi(s: &str) -> String {
    let mut paras: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in s.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if !cur.is_empty() {
                paras.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let body = trimmed.trim_start_matches(['•', '*', '·']).trim_start();
        // A wrapped continuation carried a hanging indent and no bullet; glue it
        // straight on, because CJK wraps mid-sentence with no space to restore.
        let is_continuation = !cur.is_empty()
            && trimmed.starts_with("  ")
            && !trimmed.trim_start().starts_with(['•', '*', '·']);
        if is_continuation {
            cur.push_str(body);
        } else {
            if !cur.is_empty() {
                paras.push(std::mem::take(&mut cur));
            }
            cur.push_str(body);
        }
    }
    if !cur.is_empty() {
        paras.push(cur);
    }
    // Drop leading narration: an all-ASCII paragraph in front of the real
    // (CJK-bearing) answer is Kimi talking about the task, not doing it.
    let first_cjk = paras
        .iter()
        .position(|p| p.chars().any(|c| c as u32 > 0x2E80))
        .unwrap_or(0);
    paras.drain(..first_cjk);
    paras.join("\n\n")
}

async fn version_of(bin: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
        Command::new(bin).arg("--version").stdin(Stdio::null()).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout);
    Some(v.lines().next().unwrap_or("").trim().to_string())
}

/// Which polish channels does THIS machine have? One daemon answers for the
/// whole box — the CLIs are installed per-machine, not per-bot, so there is no
/// reason to ask every daemon the user is running.
pub async fn probe_channels() -> Vec<Value> {
    let mut out = Vec::new();
    for s in SPECS {
        let version = version_of(s.bin).await;
        let installed = version.is_some();
        out.push(json!({
            "id": s.id,
            "label": s.label,
            "kind": if installed { s.kind } else { "unavailable" },
            "model": version.clone().unwrap_or_default(),
            "p50_ms": s.p50_ms,
            "enabled": installed,
            "blocked_reason": if installed { Value::Null } else { json!("未安装或未登录") },
        }));
    }
    out
}

/// Run one polish. Returns None on timeout, a non-zero exit, or empty output —
/// the caller falls through to the next channel or to the client-side pass.
pub async fn polish(channel_id: &str, level: u8, text: &str) -> Option<String> {
    let spec = SPECS.iter().find(|s| s.id == channel_id)?;
    let prompt = prompt_for(level, text);
    let started = Instant::now();

    // Every one of these needs stdin closed. `claude` and `codex` otherwise
    // block for seconds waiting on a pipe that will never carry anything —
    // measured as a flat 3s tax on claude before it gives up.
    let mut cmd = Command::new(spec.bin);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // codex writes its final message to a file; asking for it avoids parsing a
    // banner, an echo of the prompt, and a token count out of stdout.
    let tmp = std::env::temp_dir().join(format!("mafold-polish-{}.txt", std::process::id()));
    match spec.id {
        "claude-code" => {
            cmd.arg("-p").arg(&prompt);
        }
        "codex" => {
            cmd.arg("exec")
                .arg("--skip-git-repo-check")
                .arg("-o")
                .arg(&tmp)
                .arg(&prompt);
        }
        "kimi-code" => {
            // Kimi is a Python CLI: without an explicit UTF-8 codec it dies on
            // Windows with "'gbk' codec can't encode character".
            cmd.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
            cmd.arg("-p").arg(&prompt);
        }
        _ => return None,
    }

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(POLISH_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .ok()?
    .ok()?;

    let raw = if spec.id == "codex" {
        // A few KB off local disk — not worth pulling in tokio's `fs` feature.
        let s = std::fs::read_to_string(&tmp).ok()?;
        let _ = std::fs::remove_file(&tmp);
        s
    } else {
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let result = tidy(&raw, spec.id);
    println!(
        "✦ polish via {} L{} — {} → {} chars in {}ms",
        spec.id,
        level,
        text.chars().count(),
        result.chars().count(),
        started.elapsed().as_millis()
    );
    // An empty or unchanged answer is not worth animating a rewrite for.
    if result.is_empty() || result == text {
        return None;
    }
    // L1 promised not to add content. Enforce it rather than trusting the model:
    // a run where Kimi prefixed a Chinese preamble came back 2.6× the input, and
    // silently pasting that over the user's draft is worse than doing nothing.
    // (L2 restructures — headings and bullets legitimately grow the text.)
    if level < 2 && !within_l1_budget(text, &result) {
        println!("✦ polish via {} L1 rejected — {} → {} chars", spec.id, text.chars().count(), result.chars().count());
        return None;
    }
    Some(result)
}

/// A copy-edit can add punctuation and a few connectives; it cannot double the
/// draft. The slack is generous on short inputs, where a couple of commas is a
/// large relative change.
fn within_l1_budget(input: &str, output: &str) -> bool {
    let (i, o) = (input.chars().count(), output.chars().count());
    o <= (i as f64 * 1.5) as usize + 40
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_unwraps_kimi_hard_wrap() {
        // The two-space hanging indent is a terminal wrap, not a line break in
        // the text — CJK has no space to put back, so glue it.
        let raw = "• 明天下午三点开个会，讨论用户增长、留存，\n  另外把预算也定一下。";
        assert_eq!(
            tidy(raw, "kimi-code"),
            "明天下午三点开个会，讨论用户增长、留存，另外把预算也定一下。"
        );
    }

    #[test]
    fn tidy_drops_kimi_english_narration() {
        let raw = "Just polish the text.\n\n• 明天下午三点开个会，讨论用户增长。";
        assert_eq!(tidy(raw, "kimi-code"), "明天下午三点开个会，讨论用户增长。");
    }

    #[test]
    fn tidy_keeps_real_paragraph_breaks() {
        let raw = "• 第一段。\n\n• 第二段。";
        assert_eq!(tidy(raw, "kimi-code"), "第一段。\n\n第二段。");
    }

    #[test]
    fn tidy_leaves_clean_output_alone() {
        let raw = "  明天下午 3 点开个会。\n";
        assert_eq!(tidy(raw, "claude-code"), "明天下午 3 点开个会。");
    }

    #[test]
    fn tidy_strips_a_stray_code_fence() {
        let raw = "```\n明天下午 3 点开个会。\n```";
        assert_eq!(tidy(raw, "claude-code"), "明天下午 3 点开个会。");
    }

    #[test]
    fn l1_budget_allows_normal_copy_edits() {
        let a = "明天下午三点开个会讨论一下用户增长的事情还有留存也要说另外预算的问题也得定一下";
        let b = "明天下午 3 点开个会，讨论一下用户增长的事情，另外留存也要说，还有预算的问题也得定一下。";
        assert!(within_l1_budget(a, b));
    }

    #[test]
    fn l1_budget_rejects_a_ballooned_rewrite() {
        // The real failure: a preamble the model wrote about the task, glued in
        // front of the answer — 203 chars in, 533 out.
        let a = "所以呢？".repeat(50); // 200 chars
        let b = "所以呢？".repeat(140); // 560 chars
        assert!(!within_l1_budget(&a, &b));
    }

    #[test]
    fn l2_prompt_allows_restructuring_l1_does_not() {
        assert!(prompt_for(1, "x").contains("不要改变原意"));
        assert!(prompt_for(2, "x").contains("重新组织"));
    }
}

/// Dev harness: `MAFOLD_POLISH_SELFTEST=1 mafold --help` runs the real CLIs
/// against short / medium / long drafts and prints what came back. Not wired
/// into any command — it exists so the polish path can be exercised end to end
/// without standing up the API.
pub async fn selftest() {
    const SHORT: &str = "明天下午三点开个会讨论一下用户增长的事情还有留存也要说另外预算的问题也得定一下";
    const MED: &str = "那个登陆页面在手机上打不开报错说什么token过期了我清了缓存也不行昨天还好好的另外我发现侧边栏在小屏幕上会挡住内容深色模式下有几个图标也看不清这些是不是可以一起修了";
    const LONG: &str = "所以呢？现在给你绝对充足的时间，你现在在这三个方向上进一步思考，并且去做真实调研，和我全面的说一说你接下来的计划。我个人的想法是，私人收藏为体的体验不好，不等于联网为体的体验就好，到底以什么为体是不是可以更动态？或者能不能是一趟长程任务？或者有没有可能从用户收藏出发再去联网？我们的抽卡精确用户想法机制是不是能在更多地方发挥作用？这些都需要你去更多的发散、想象、收敛、调研，思考、评估、设计！不要闭门造车。";
    println!("── probe ──");
    for c in probe_channels().await {
        println!("  {}", c);
    }
    for (name, text) in [("short", SHORT), ("med", MED), ("long", LONG)] {
        for ch in ["kimi-code", "claude-code", "codex"] {
            let t = Instant::now();
            let out = polish(ch, 1, text).await;
            println!(
                "── L1 {ch} / {name} — {}ms\n   {}",
                t.elapsed().as_millis(),
                out.as_deref().unwrap_or("<none>").chars().take(120).collect::<String>()
            );
        }
    }
    let t = Instant::now();
    let out = polish("claude-code", 2, LONG).await;
    println!("── L2 claude-code / long — {}ms\n{}", t.elapsed().as_millis(), out.unwrap_or_default());
}
