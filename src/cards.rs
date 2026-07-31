//! `mafold cards …` — author, preview, and publish developer cards.
//!
//!   mafold cards init <name>     # scaffold a card project
//!   mafold cards dev             # bundle + watch + serve locally (live preview)
//!   mafold cards publish         # bundle + upload to Mafold
//!   mafold cards list            # your cards + global cards
//!   mafold cards unpublish <tag> # retract it from your scope (clears a shadow)
//!
//! Cards are real React Native components (rendered via react-native-web on the
//! web, Hermes on iOS). We bundle them with a bundled esbuild — fetched once to
//! ~/.mafold/bin/esbuild — so authors need no Node/npm toolchain (decision A).
//! react / react-native / @mafold/cards are left external and injected by the
//! host runtime at load time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::json;

use crate::client::Client;

/// Pinned esbuild version fetched on demand.
const ESBUILD_VERSION: &str = "0.24.2";
/// Imports left for the host runtime to provide.
const EXTERNALS: &[&str] = &[
    "react",
    "react/jsx-runtime",
    "react-native",
    // Native module the host provides — lucide-react-native renders through it.
    "react-native-svg",
    "@mafold/cards",
];

#[derive(Subcommand)]
pub enum CardsCmd {
    /// Scaffold a new card project in ./<name>.
    Init {
        /// Card tag (lowercase, e.g. `kline`). Also the folder name.
        name: String,
        /// Parent directory to create the project in (default: current dir).
        #[arg(long, default_value = ".")]
        dir: String,
    },
    /// Bundle + watch + serve the card locally for live preview.
    Dev {
        /// Card project directory (must contain mafold.card.json).
        #[arg(long, default_value = ".")]
        dir: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Bundle and publish the card to Mafold.
    Publish {
        #[arg(long, default_value = ".")]
        dir: String,
    },
    /// List your cards plus global cards.
    List,
    /// Retract a card from YOUR scope. Without `--version` the whole tag goes,
    /// so a tag you accidentally published over resolves to the global card
    /// again; with `--version` only that one version goes (a rollback).
    Unpublish {
        /// Card tag, e.g. `bash`.
        tag: String,
        /// Retract only this version instead of the whole tag.
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Deserialize)]
struct CardManifest {
    tag: String,
    version: String,
    #[serde(default = "default_entry")]
    entry: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}
fn default_entry() -> String {
    "src/card.tsx".into()
}

pub async fn run(cmd: CardsCmd, base: String, token: Option<String>) -> Result<()> {
    match cmd {
        CardsCmd::Init { name, dir } => cmd_init(&name, &dir),
        CardsCmd::Dev { dir, port } => cmd_dev(&dir, port).await,
        CardsCmd::Publish { dir } => cmd_publish(&dir, base, token).await,
        CardsCmd::List => cmd_list(base, token).await,
        CardsCmd::Unpublish { tag, version } => {
            cmd_unpublish(&tag, version.as_deref(), base, token).await
        }
    }
}

// ───────────────────────────── init ─────────────────────────────

fn cmd_init(name: &str, dir: &str) -> Result<()> {
    if !valid_tag(name) {
        anyhow::bail!("name must be a lowercase tag matching [a-z][a-z0-9-]* (e.g. `kline`)");
    }
    let root = Path::new(dir).join(name);
    if root.exists() {
        anyhow::bail!("{} already exists", root.display());
    }
    std::fs::create_dir_all(root.join("src"))?;

    let title = title_case(name);
    write(&root.join("mafold.card.json"), &manifest_json(name, &title))?;
    write(&root.join("src/card.tsx"), &sample_card(name))?;
    write(&root.join("package.json"), &package_json(name))?;
    write(&root.join("README.md"), &readme(name))?;
    write(&root.join(".gitignore"), "dist/\nnode_modules/\n")?;

    println!("✓ created card `{name}` in {}", root.display());
    println!("\nnext:");
    println!("  cd {}", root.display());
    println!("  mafold cards dev               # live preview at http://127.0.0.1:8787");
    println!("  mafold cards publish           # ship it (needs your bot token)");
    Ok(())
}

// ───────────────────────────── dev ─────────────────────────────

async fn cmd_dev(dir: &str, port: u16) -> Result<()> {
    let manifest = read_manifest(dir)?;
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }
    let esbuild = ensure_esbuild().await?;
    let out = format!("dist/{}.js", manifest.tag);

    println!("→ serving {} on http://127.0.0.1:{port}/{}.js", manifest.tag, manifest.tag);
    println!("  watching {} (Ctrl-C to stop)\n", manifest.entry);

    // esbuild runs the watch + static server itself; this blocks until Ctrl-C.
    let mut c = tokio::process::Command::new(&esbuild);
    c.current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args(&out))
        .arg("--watch")
        .arg("--servedir=dist")
        .arg(format!("--serve=127.0.0.1:{port}"));
    let status = c.status().await.context("failed to run esbuild")?;
    if !status.success() {
        anyhow::bail!("esbuild exited with {status}");
    }
    Ok(())
}

// ───────────────────────────── publish ─────────────────────────────

async fn cmd_publish(dir: &str, base: String, token: Option<String>) -> Result<()> {
    let token = token.context(
        "publish needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN",
    )?;
    let manifest = read_manifest(dir)?;
    if !valid_tag(&manifest.tag) {
        anyhow::bail!("mafold.card.json tag must match [a-z][a-z0-9-]*");
    }
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }

    let esbuild = ensure_esbuild().await?;
    let out_dir = Path::new(dir).join("dist");
    std::fs::create_dir_all(&out_dir)?;
    let out = out_dir.join(format!("{}.js", manifest.tag));

    println!("→ bundling {} …", manifest.entry);
    let status = tokio::process::Command::new(&esbuild)
        .current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args(&format!("dist/{}.js", manifest.tag)))
        .arg("--minify")
        .status()
        .await
        .context("failed to run esbuild")?;
    if !status.success() {
        anyhow::bail!("esbuild exited with {status}");
    }
    let bundle = std::fs::read(&out).with_context(|| format!("reading {}", out.display()))?;
    println!("  bundle: {} ({:.1} KB)", out.display(), bundle.len() as f64 / 1024.0);

    let client = Client::new(base, token);
    let meta = json!({
        "tag": manifest.tag,
        "version": manifest.version,
        "display_name": manifest.display_name,
    });
    let r = client.publish_card(&meta, bundle).await?;
    let scope = r["scope"].as_str().unwrap_or("?");
    let url = r["url"].as_str().unwrap_or("?");
    // The server owns the stored version: it may auto-bump past the manifest
    // label when the content drifted, or return an older label as a no-op.
    let version = r["version"].as_str().unwrap_or(&manifest.version);
    println!(
        "\n✓ published {}@{} ({} scope)\n  url:   {}\n  use:   {{% {} /%}}",
        manifest.tag, version, scope, url, manifest.tag
    );
    if version != manifest.version {
        println!("  note: server stored {version} (manifest says {}) — content-drift auto-bump", manifest.version);
    }
    // The accident that has no symptoms: publishing a first-party tag with your
    // OWN token succeeds, looks identical to a real publish, and then freezes
    // that tag for your whole family while the pipeline keeps shipping a global
    // copy you no longer resolve. Say it loudly, and say how to undo it.
    if r["shadows_global"].as_bool().unwrap_or(false) {
        println!(
            "\n⚠ this SHADOWS the global `{}` card for everyone in the `{scope}` family.\n  \
             They will stop receiving global updates to it until you run:\n      \
             mafold cards unpublish {}",
            manifest.tag, manifest.tag
        );
    }
    Ok(())
}

// ───────────────────────────── unpublish ─────────────────────────────

async fn cmd_unpublish(
    tag: &str,
    version: Option<&str>,
    base: String,
    token: Option<String>,
) -> Result<()> {
    let token = token
        .context("unpublish needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.unpublish_card(tag, version).await?;
    let scope = r["scope"].as_str().unwrap_or("?");
    let removed: Vec<&str> = r["removed"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    println!(
        "✓ retracted {tag} {} from the {scope} scope: {}",
        if removed.len() == 1 { "version" } else { "versions" },
        removed.join(", "),
    );
    // What it resolves to NOW is the only answer that matters — clearing a
    // shadow is pointless if nothing falls in behind it.
    match r["now_resolves_to"].as_object() {
        Some(n) => println!(
            "  {{% {tag} /%}} now resolves to {} [{}]",
            n["version"].as_str().unwrap_or("?"),
            n["scope"].as_str().unwrap_or("?"),
        ),
        None => println!(
            "  ⚠ {{% {tag} /%}} now resolves to NOTHING{} — clients without a cached\n  \
             copy will render it as unavailable. Publishing again brings it back.",
            if scope == "global" { ", for every account" } else { ", and there is no global card to fall back to" },
        ),
    }
    // Retracted labels stay spent: clients refetch only when the version string
    // moves, so handing an old label to new bytes would strand everyone holding
    // the old ones. Say so, because the next publish will visibly skip a number.
    println!("  the retracted version number{} will not be reused — the next publish climbs past {}",
        if removed.len() == 1 { "" } else { "s" },
        removed.last().copied().unwrap_or("it"),
    );
    Ok(())
}

// ───────────────────────────── list ─────────────────────────────

async fn cmd_list(base: String, token: Option<String>) -> Result<()> {
    let token = token.context("list needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.list_cards().await?;
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no cards)");
        return Ok(());
    }
    for c in items {
        let tag = c["tag"].as_str().unwrap_or("?");
        let ver = c["version"].as_str().unwrap_or("?");
        let scope = c["scope"].as_str().unwrap_or("?");
        println!("• {{% {tag} /%}}  {ver}  [{scope}]");
    }
    Ok(())
}

// ───────────────────────────── esbuild ─────────────────────────────

/// Build the esbuild bundle args. `extra_externals` lets the apps pipeline add
/// `@mafold/app` / `@mafold/runtime-core` to the host-provided imports without
/// forking the bundler invocation (see §「落地映射」of the unified-runtime spec).
pub(crate) fn bundle_args_with(outfile: &str, extra_externals: &[&str]) -> Vec<String> {
    let mut a = vec![
        "--bundle".into(),
        "--format=cjs".into(),
        "--platform=browser".into(),
        "--jsx=automatic".into(),
        format!("--outfile={outfile}"),
    ];
    for e in EXTERNALS.iter().chain(extra_externals.iter()) {
        a.push(format!("--external:{e}"));
    }
    a
}

fn bundle_args(outfile: &str) -> Vec<String> {
    bundle_args_with(outfile, &[])
}

/// esbuild's per-platform npm package (the binary lives at package/bin/esbuild).
fn esbuild_platform() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("darwin-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("darwin-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-arm64")
    } else {
        None
    }
}

fn mafold_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".mafold")
}

/// Ensure a usable esbuild binary exists, fetching it once to ~/.mafold/bin.
///
/// Concurrency-safe: several `mafold cards publish` processes routinely run at
/// once (the cards pipeline publishes the whole set in parallel), and on a cold
/// machine they all miss the cache together. Every process therefore stages its
/// download in a PRIVATE directory, chmods there, and only then renames into
/// place — an atomic swap on the same filesystem. Losers of the race just
/// overwrite with identical bytes.
///
/// The previous version shared one staging path, chmodded only after the rename,
/// and deleted the staging dir at the end — so racing processes hit
/// `Text file busy` (exec'ing a file another process still had open for writing)
/// and `No such file or directory` (their extracted file removed underneath).
pub(crate) async fn ensure_esbuild() -> Result<PathBuf> {
    let bin = mafold_home().join("bin").join("esbuild");
    if bin.exists() {
        return Ok(bin);
    }
    let plat = esbuild_platform().context("no esbuild build for this platform")?;
    let dir = bin.parent().unwrap().to_path_buf();
    // Private per-process staging area; nothing here is shared with a sibling.
    let stage = dir.join(format!(".esbuild-stage-{}", std::process::id()));
    std::fs::create_dir_all(&stage)?;
    // Best-effort cleanup on every exit path below.
    let cleanup = |stage: &Path| {
        let _ = std::fs::remove_dir_all(stage);
    };

    let url = format!("https://registry.npmjs.org/@esbuild/{plat}/-/{plat}-{ESBUILD_VERSION}.tgz");
    eprintln!("→ fetching esbuild {ESBUILD_VERSION} ({plat})…");
    let fetched = async {
        let bytes = reqwest::Client::new()
            .get(&url)
            .header("User-Agent", "mafold-cli")
            .send()
            .await?
            .error_for_status()
            .context("esbuild download failed")?
            .bytes()
            .await?;

        let tgz = stage.join("esbuild.tgz");
        std::fs::write(&tgz, &bytes)?;
        // The npm tarball stores the binary at package/bin/esbuild.
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&tgz)
            .arg("-C")
            .arg(&stage)
            .arg("package/bin/esbuild")
            .status()
            .context("tar not found — install tar or esbuild manually")?;
        if !status.success() {
            anyhow::bail!("failed to extract esbuild");
        }
        let staged = stage.join("package/bin/esbuild");
        // chmod BEFORE publishing it: once the rename lands, a sibling may exec
        // it immediately.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&staged, &bin)?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    cleanup(&stage);
    fetched?;
    Ok(bin)
}

// ───────────────────────────── helpers ─────────────────────────────

fn read_manifest(dir: &str) -> Result<CardManifest> {
    let path = Path::new(dir).join("mafold.card.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no mafold.card.json in {dir} — run `mafold cards init` first"))?;
    serde_json::from_str(&text).context("mafold.card.json is not valid JSON")
}

pub(crate) fn write(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(crate) fn title_case(tag: &str) -> String {
    tag.split('-')
        .map(|w| {
            let mut ch = w.chars();
            match ch.next() {
                Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn manifest_json(tag: &str, title: &str) -> String {
    format!(
        "{{\n  \"tag\": \"{tag}\",\n  \"version\": \"0.1.0\",\n  \"entry\": \"src/card.tsx\",\n  \"displayName\": \"{title}\"\n}}\n"
    )
}

fn package_json(tag: &str) -> String {
    format!(
        "{{\n  \"name\": \"{tag}-card\",\n  \"private\": true,\n  \"version\": \"0.1.0\",\n  \"peerDependencies\": {{\n    \"react\": \"*\",\n    \"react-native\": \"*\"\n  }}\n}}\n"
    )
}

fn sample_card(tag: &str) -> String {
    // Automatic JSX, so no `import React` is needed. react-native is provided by
    // the host (react-native-web on web); @mafold/cards exposes the card SDK.
    format!(
        r#"import {{ View, Text, Pressable, StyleSheet }} from "react-native";
import {{ defineCard, useHost }} from "@mafold/cards";

function {title}({{ name = "world" }}: {{ name?: string }}) {{
  const {{ theme, sendAction }} = useHost();
  const t = theme.tokens;
  return (
    <View style={{[styles.card, {{ backgroundColor: t.float, borderColor: t.border }}]}}>
      <Text style={{[styles.title, {{ color: t.text }}]}}>Hello, {{name}} 👋</Text>
      <Pressable
        style={{[styles.btn, {{ backgroundColor: t.accent }}]}}
        onPress={{() => sendAction("tap", {{ name }})}}
      >
        <Text style={{{{ color: t.onAccent, fontWeight: "600" }}}}>Tap me</Text>
      </Pressable>
    </View>
  );
}}

const styles = StyleSheet.create({{
  card: {{ padding: 14, borderRadius: 12, borderWidth: StyleSheet.hairlineWidth, gap: 10 }},
  title: {{ fontSize: 16, fontWeight: "700" }},
  btn: {{ paddingVertical: 9, borderRadius: 9999, alignItems: "center" }},
}});

export default defineCard({{
  tag: "{tag}",
  attributes: {{ name: {{ type: "string" }} }},
  // examples power the detail-page preview and show others how to call the card.
  // Each is {{ name, props, description? }} — add one per state worth showing.
  examples: [
    {{ name: "Default", props: {{ name: "world" }} }},
    {{ name: "Named", props: {{ name: "Mafold" }}, description: "with a custom name" }},
  ],
  component: {title},
}});
"#,
        title = title_case(tag).replace(' ', ""),
        tag = tag,
    )
}

fn readme(tag: &str) -> String {
    format!(
        "# {tag} card\n\nA Mafold card written in React Native.\n\n```\nmafold cards dev       # live preview\nmafold cards publish   # ship it\n```\n\nUse it in a message: `{{% {tag} /%}}`\n"
    )
}
