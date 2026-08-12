//! `mafold langpack …` — publish the cloud language packs (`langpacks/*.json`).
//!
//!   mafold langpack publish                 # publish every <lang>.json (bump version)
//!   mafold langpack publish --lang zh-Hans  # just one
//!   mafold langpack publish --version 5      # force a version
//!   mafold langpack list                    # what the server serves now
//!
//! The official pack is first-party: publish needs the publisher token (the
//! `mafoldcards` account). One pack covers every surface (site/web/iOS/RN);
//! versions are integers so clients sync only the delta. See .docs/i18n-v0.md.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;

/// Human-readable names for shipped languages; falls back to the bare code.
fn lang_name(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some("English"),
        "zh-Hans" => Some("简体中文"),
        "zh-Hant" => Some("繁體中文"),
        "ja" => Some("日本語"),
        "ko" => Some("한국어"),
        "es" => Some("Español"),
        "fr" => Some("Français"),
        "de" => Some("Deutsch"),
        "pt-BR" => Some("Português (Brasil)"),
        "ru" => Some("Русский"),
        "ar" => Some("العربية"),
        _ => None,
    }
}

/// Right-to-left scripts (drives the client's layout direction).
fn is_rtl(code: &str) -> bool {
    matches!(code.split('-').next(), Some("ar" | "he" | "fa" | "ur"))
}

#[derive(Subcommand)]
pub enum LangpackCmd {
    /// Bundle + publish each `<dir>/<lang>.json` to Mafold (first-party only).
    Publish {
        /// Directory of `<lang>.json` packs.
        #[arg(long, default_value = "langpacks")]
        dir: String,
        /// Publish only this language (default: every `<lang>.json` in `--dir`).
        #[arg(long)]
        lang: Option<String>,
        /// Force this integer version for all (default: current server version + 1).
        #[arg(long)]
        version: Option<u32>,
    },
    /// List the languages the server currently serves.
    List,
}

pub async fn run(cmd: LangpackCmd, base: String, token: Option<String>) -> Result<()> {
    match cmd {
        LangpackCmd::Publish { dir, lang, version } => {
            cmd_publish(&dir, lang, version, base, token).await
        }
        LangpackCmd::List => cmd_list(base, token).await,
    }
}

fn require_token(token: Option<String>) -> Result<String> {
    token.context(
        "publish needs the first-party publisher token — pass --token or set $MAFOLD_BOT_TOKEN",
    )
}

/// The server's current version for `code` (0 if it has never been published).
async fn current_version(client: &Client, code: &str) -> Result<u32> {
    let r = client
        .resolve_langpacks(json!([{ "lang_code": code }]))
        .await?;
    Ok(r["entries"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e["version"].as_u64())
        .unwrap_or(0) as u32)
}

async fn cmd_publish(
    dir: &str,
    only: Option<String>,
    version: Option<u32>,
    base: String,
    token: Option<String>,
) -> Result<()> {
    let client = Client::new(base, require_token(token)?);

    // Collect (lang_code, strings) from `<dir>/<lang>.json`.
    let mut packs: Vec<(String, Value)> = Vec::new();
    if let Some(l) = &only {
        let p = Path::new(dir).join(format!("{l}.json"));
        let s = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        packs.push((l.clone(), parse_pack(&s, l)?));
    } else {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {dir}"))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // `_meta.json` and friends, plus DOTFILES: `.seed-lock.json` has a
            // `.json` extension and a `.seed-lock` stem, so without this it
            // would be published as a language named ".seed-lock".
            if stem.starts_with('_') || stem.starts_with('.') {
                continue;
            }
            let s = std::fs::read_to_string(&path)?;
            packs.push((stem.to_string(), parse_pack(&s, stem)?));
        }
    }
    if packs.is_empty() {
        anyhow::bail!("no <lang>.json packs found in {dir}");
    }
    packs.sort_by(|a, b| a.0.cmp(&b.0));

    for (code, strings) in packs {
        let key_count = strings.as_object().map(|o| o.len()).unwrap_or(0);
        let ver = match version {
            Some(v) => v,
            None => current_version(&client, &code).await? + 1,
        };
        let mut body = json!({
            "lang_code": code,
            "version": ver,
            "rtl": is_rtl(&code),
            "strings": strings,
        });
        if let Some(name) = lang_name(&code) {
            body["name"] = json!(name);
        }
        let r = client.publish_langpack(body).await?;
        println!(
            "✓ published {code} v{ver} ({key_count} keys) → {}",
            r["url"].as_str().unwrap_or("?")
        );
    }
    // "next launch" was true until the web client grew card-style background
    // revalidation; saying it still would understate the pipeline and send
    // people reloading to check their own publish.
    println!(
        "\nopen clients re-sync within ~60s (or on tab focus); others on next launch — \
         no app release needed."
    );
    Ok(())
}

/// Parse + validate a pack file: it must be a JSON object of `key -> value`.
fn parse_pack(s: &str, lang: &str) -> Result<Value> {
    let v: Value =
        serde_json::from_str(s).with_context(|| format!("{lang}.json is not valid JSON"))?;
    if !v.is_object() {
        anyhow::bail!("{lang}.json must be a JSON object of key -> value");
    }
    Ok(v)
}

async fn cmd_list(base: String, token: Option<String>) -> Result<()> {
    let client = Client::new(base, require_token(token)?);
    let r = client.list_languages().await?;
    let langs = r["languages"].as_array().cloned().unwrap_or_default();
    if langs.is_empty() {
        println!("no languages published yet — run `mafold langpack publish`.");
        return Ok(());
    }
    println!("languages on the server:");
    for l in langs {
        println!(
            "  {:<8} v{:<3} {}{}",
            l["code"].as_str().unwrap_or("?"),
            l["version"].as_u64().unwrap_or(0),
            l["name"].as_str().unwrap_or(""),
            if l["rtl"].as_bool() == Some(true) {
                "  (rtl)"
            } else {
                ""
            },
        );
    }
    Ok(())
}
