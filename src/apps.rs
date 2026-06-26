//! `mafold apps …` — author, preview, and publish developer mini-apps.
//!
//!   mafold apps init <owner/slug>   # scaffold an app project
//!   mafold apps dev                 # bundle + watch + serve locally (live preview)
//!   mafold apps publish             # bundle + upload to Mafold
//!   mafold apps list                # the apps you can manage
//!   mafold apps remove <owner/slug> # take down an app (all versions)
//!
//! Apps are the interactive counterpart to cards: full React Native screens,
//! launched from a chat, talking to the host through the `useApp()` bridge.
//! Ownership is by ACCOUNT and the app-id is `owner/slug` (e.g. `mafold/wallet`,
//! `ops:bot/notes`) — reverse-DNS is dead (see docs/unified-runtime-v0.md).
//!
//! This mirrors the cards pipeline (`cards.rs`): the same bundled esbuild with
//! externals injected by the host runtime, the same multipart upload, the same
//! token handling. Apps additionally externalize `@mafold/app` /
//! `@mafold/runtime-core` (the interactive SDK profile) on top of the card set.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cards::{bundle_args_with, ensure_esbuild, title_case, write};
use crate::client::Client;

/// Imports the apps pipeline externalizes ON TOP of the card externals — the
/// interactive SDK profile the host injects at load time.
const APP_EXTERNALS: &[&str] = &["@mafold/app", "@mafold/runtime-core"];

#[derive(Subcommand)]
pub enum AppsCmd {
    /// Scaffold a new mini-app project. `id` is `owner/slug` (e.g. `ops/todo`);
    /// the project folder is named after the slug.
    Init {
        /// App id, `owner/slug` (e.g. `mafold/wallet`, `ops:bot/notes`).
        id: String,
        /// Parent directory to create the project in (default: current dir).
        #[arg(long, default_value = ".")]
        dir: String,
    },
    /// Bundle + watch + serve the app locally for live preview.
    Dev {
        /// App project directory (must contain mafold.app.json).
        #[arg(long, default_value = ".")]
        dir: String,
        #[arg(long, default_value_t = 8788)]
        port: u16,
    },
    /// Bundle and publish the app to Mafold. Gated: you must own the namespace.
    Publish {
        #[arg(long, default_value = ".")]
        dir: String,
    },
    /// List the apps you can manage (own the namespace of).
    List,
    /// Take down an app you own (all versions). `id` is `owner/slug`.
    Remove {
        /// App id, `owner/slug`.
        id: String,
    },
}

/// The on-disk app manifest (`mafold.app.json`). Sent verbatim as the publish
/// `meta`; the server reads `id` + `version` and stores the rest opaquely, so
/// this is a client contract. Mirrors `cards/sdk-app/index.d.ts::AppManifest`.
#[derive(Deserialize)]
struct AppManifest {
    /// GLOBAL id, `owner/slug`.
    id: String,
    version: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(rename = "kind", default)]
    kind: Option<String>,
    #[serde(default = "default_entry")]
    entry: String,
    /// Anything else in the manifest (panel/capabilities/tag/…) is preserved and
    /// re-uploaded verbatim so we don't drop fields the server stores opaquely.
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}
fn default_entry() -> String {
    "src/app.tsx".into()
}

pub async fn run(cmd: AppsCmd, base: String, token: Option<String>) -> Result<()> {
    match cmd {
        AppsCmd::Init { id, dir } => cmd_init(&id, &dir),
        AppsCmd::Dev { dir, port } => cmd_dev(&dir, port).await,
        AppsCmd::Publish { dir } => cmd_publish(&dir, base, token).await,
        AppsCmd::List => cmd_list(base, token).await,
        AppsCmd::Remove { id } => cmd_remove(&id, base, token).await,
    }
}

// ───────────────────────────── init ─────────────────────────────

fn cmd_init(id: &str, dir: &str) -> Result<()> {
    let (owner, slug) = parse_app_id(id)
        .context("id must be `owner/slug` (e.g. `mafold/wallet`); reverse-DNS is no longer valid")?;
    let root = Path::new(dir).join(&slug);
    if root.exists() {
        anyhow::bail!("{} already exists", root.display());
    }
    std::fs::create_dir_all(root.join("src"))?;

    let title = title_case(&slug);
    write(&root.join("mafold.app.json"), &manifest_json(id, &title))?;
    write(&root.join("src/app.tsx"), &sample_app(&slug, &title))?;
    write(&root.join("package.json"), &package_json(&slug))?;
    write(&root.join("README.md"), &readme(id, &slug))?;
    write(&root.join(".gitignore"), "dist/\nnode_modules/\n")?;

    println!("✓ created app `{owner}/{slug}` in {}", root.display());
    println!("\nnext:");
    println!("  cd {}", root.display());
    println!("  mafold apps dev               # live preview at http://127.0.0.1:8788");
    println!("  mafold apps publish           # ship it (needs your bot token; you must own `{owner}`)");
    Ok(())
}

// ───────────────────────────── dev ─────────────────────────────

async fn cmd_dev(dir: &str, port: u16) -> Result<()> {
    let manifest = read_manifest(dir)?;
    let (_owner, slug) = parse_app_id(&manifest.id)
        .context("mafold.app.json id must be `owner/slug`")?;
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }
    let esbuild = ensure_esbuild().await?;
    let out = format!("dist/{slug}.js");

    println!("→ serving {} on http://127.0.0.1:{port}/{slug}.js", manifest.id);
    println!("  watching {} (Ctrl-C to stop)\n", manifest.entry);

    let mut c = tokio::process::Command::new(&esbuild);
    c.current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args_with(&out, APP_EXTERNALS))
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
    let mut manifest = read_manifest(dir)?;
    let (_owner, slug) = parse_app_id(&manifest.id)
        .context("mafold.app.json id must be `owner/slug` (reverse-DNS is no longer valid)")?;
    if manifest.version.trim().is_empty() {
        anyhow::bail!("mafold.app.json version must be a non-empty string");
    }
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }

    let client = Client::new(base, token);

    // If `icon` points at a LOCAL image file (relative to the project dir),
    // upload it and rewrite `icon` to the served `/media/…` path so the published
    // manifest carries a real logo. A lucide glyph name or an existing URL is
    // left untouched. See apps/AppIcon.tsx (web renders either kind).
    if let Some(icon) = manifest.icon.clone() {
        if let Some(path) = local_logo_path(dir, &icon) {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading icon {}", path.display()))?;
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("logo");
            println!("→ uploading logo {} ({:.1} KB) …", icon, bytes.len() as f64 / 1024.0);
            let r = client.upload_media(bytes, fname, mime_for(fname)).await?;
            let url = r["url"].as_str().context("uploadFile returned no url")?.to_string();
            println!("  logo:  {url}");
            manifest.icon = Some(url);
        }
    }

    let esbuild = ensure_esbuild().await?;
    let out_dir = Path::new(dir).join("dist");
    std::fs::create_dir_all(&out_dir)?;
    let out = out_dir.join(format!("{slug}.js"));

    println!("→ bundling {} …", manifest.entry);
    let status = tokio::process::Command::new(&esbuild)
        .current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args_with(&format!("dist/{slug}.js"), APP_EXTERNALS))
        .arg("--minify")
        .status()
        .await
        .context("failed to run esbuild")?;
    if !status.success() {
        anyhow::bail!("esbuild exited with {status}");
    }
    let bundle = std::fs::read(&out).with_context(|| format!("reading {}", out.display()))?;
    println!("  bundle: {} ({:.1} KB)", out.display(), bundle.len() as f64 / 1024.0);

    // The server stores the manifest verbatim, so send the FULL manifest JSON
    // (not a hand-picked subset) — reconstruct it from the parsed fields + the
    // flattened `rest` so panel/capabilities/tag survive.
    let meta = manifest.to_json();
    let r = client.publish_app(&meta, bundle).await?;
    let id = r["id"].as_str().unwrap_or(&manifest.id);
    let version = r["version"].as_str().unwrap_or(&manifest.version);
    let url = r["url"].as_str().unwrap_or("?");
    println!("\n✓ published {id}@{version}\n  url:   {url}");
    Ok(())
}

// ───────────────────────────── list ─────────────────────────────

async fn cmd_list(base: String, token: Option<String>) -> Result<()> {
    let token =
        token.context("list needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.list_apps().await?;
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no apps)");
        return Ok(());
    }
    for a in items {
        let id = a["id"].as_str().unwrap_or("?");
        let ver = a["version"].as_str().unwrap_or("?");
        let name = a["manifest"]["name"].as_str().unwrap_or("");
        if name.is_empty() {
            println!("• {id}  {ver}");
        } else {
            println!("• {id}  {ver}  ({name})");
        }
    }
    Ok(())
}

// ───────────────────────────── remove ─────────────────────────────

async fn cmd_remove(id: &str, base: String, token: Option<String>) -> Result<()> {
    let token =
        token.context("remove needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    // Validate the shape locally for a friendlier error; the server re-checks
    // both the shape and that you own the namespace.
    parse_app_id(id).context("id must be `owner/slug`")?;
    let client = Client::new(base, token);
    let r = client.remove_app(id).await?;
    if r["removed"].as_bool() == Some(true) {
        println!("✓ removed {id} (all versions)");
    } else {
        println!("• nothing to remove for {id} (no such app, or already gone)");
    }
    Ok(())
}

// ───────────────────────────── helpers ─────────────────────────────

impl AppManifest {
    /// Reassemble the manifest JSON exactly as authored: the named fields plus
    /// every flattened-through field. Sent verbatim as the publish `meta`.
    fn to_json(&self) -> Value {
        let mut m = self.rest.clone();
        m.insert("id".into(), json!(self.id));
        m.insert("version".into(), json!(self.version));
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        if let Some(i) = &self.icon {
            m.insert("icon".into(), json!(i));
        }
        if let Some(k) = &self.kind {
            m.insert("kind".into(), json!(k));
        }
        m.insert("entry".into(), json!(self.entry));
        Value::Object(m)
    }
}

/// If `icon` names a LOCAL image file (relative to the project dir) — not a
/// lucide glyph name and not a URL/data URI — return its path. Requires the file
/// to exist with a known image extension.
fn local_logo_path(dir: &str, icon: &str) -> Option<std::path::PathBuf> {
    if icon.starts_with("http://") || icon.starts_with("https://") || icon.starts_with("data:") {
        return None;
    }
    let ext = Path::new(icon).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    let is_image = matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "svg" | "webp" | "gif" | "avif"));
    if !is_image {
        return None;
    }
    let path = Path::new(dir).join(icon);
    path.is_file().then_some(path)
}

/// Best-effort content-type from a filename extension (the server also re-derives
/// + gates the stored extension).
fn mime_for(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    }
}

fn read_manifest(dir: &str) -> Result<AppManifest> {
    let path = Path::new(dir).join("mafold.app.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no mafold.app.json in {dir} — run `mafold apps init` first"))?;
    serde_json::from_str(&text).context("mafold.app.json is not valid JSON")
}

/// `owner/slug` → (owner, slug), validating both halves. Mirrors the server's
/// `apps_api.rs::parse_app_id` exactly so a publish that would be rejected fails
/// here with a clearer message (the server stays authoritative — it re-checks
/// the shape AND namespace ownership).
fn parse_app_id(id: &str) -> Option<(String, String)> {
    let (owner, slug) = id.split_once('/')?;
    if valid_owner(owner) && valid_slug(slug) {
        Some((owner.to_string(), slug.to_string()))
    } else {
        None
    }
}

/// Account-username shape, allowing one `:` for namespaced accounts (`mafold:ai`).
fn valid_owner(owner: &str) -> bool {
    if owner.is_empty() || owner.len() > 64 {
        return false;
    }
    let segs: Vec<&str> = owner.split(':').collect();
    if segs.len() > 2 {
        return false;
    }
    segs.iter().all(|s| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// `[a-z][a-z0-9-]*`, ≤64 — same shape as a card tag / the server's `valid_slug`.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn manifest_json(id: &str, title: &str) -> String {
    format!(
        "{{\n  \"id\": \"{id}\",\n  \"name\": \"{title}\",\n  \"icon\": \"app-window\",\n  \"version\": \"0.1.0\",\n  \"entry\": \"src/app.tsx\",\n  \"panel\": {{ \"size\": \"tall\" }},\n  \"capabilities\": [\"storage\"]\n}}\n"
    )
}

fn package_json(slug: &str) -> String {
    format!(
        "{{\n  \"name\": \"{slug}-app\",\n  \"private\": true,\n  \"version\": \"0.1.0\",\n  \"peerDependencies\": {{\n    \"react\": \"*\",\n    \"react-native\": \"*\"\n  }}\n}}\n"
    )
}

fn sample_app(slug: &str, title: &str) -> String {
    // Automatic JSX, so no `import React` for the runtime. react-native is
    // provided by the host (react-native-web on web, Hermes on iOS); @mafold/app
    // exposes the interactive SDK (defineApp / useApp).
    let comp = title.replace(' ', "");
    format!(
        r#"import {{ View, Text, Pressable, StyleSheet }} from "react-native";
import {{ defineApp, useApp }} from "@mafold/app";

function {comp}() {{
  const app = useApp();
  const t = app.theme.tokens;
  const {{ me }} = app.context;
  return (
    <View style={{[styles.screen, {{ backgroundColor: t.bg }}]}}>
      <Text style={{[styles.title, {{ color: t.text }}]}}>Hello, {{me.displayName}} 👋</Text>
      <Text style={{[styles.sub, {{ color: t.muted }}]}}>{title} mini-app</Text>
      <Pressable
        style={{[styles.btn, {{ backgroundColor: t.accent }}]}}
        onPress={{() => app.ui.close()}}
      >
        <Text style={{{{ color: t.onAccent, fontWeight: "600" }}}}>Close</Text>
      </Pressable>
    </View>
  );
}}

const styles = StyleSheet.create({{
  screen: {{ flex: 1, padding: 20, gap: 12, justifyContent: "center" }},
  title: {{ fontSize: 22, fontWeight: "700" }},
  sub: {{ fontSize: 14 }},
  btn: {{ marginTop: 8, paddingVertical: 11, borderRadius: 12, alignItems: "center" }},
}});

export default defineApp(
  {{
    id: "{slug}",
    name: "{title}",
    icon: "app-window",
    version: "0.1.0",
    entry: "src/app.tsx",
  }},
  {comp},
);
"#,
        comp = comp,
        slug = slug,
        title = title,
    )
}

fn readme(id: &str, slug: &str) -> String {
    format!(
        "# {slug} app\n\nA Mafold mini-app written in React Native (`{id}`).\n\n```\nmafold apps dev       # live preview\nmafold apps publish   # ship it (you must own the namespace)\nmafold apps list      # the apps you can manage\nmafold apps remove {id}\n```\n"
    )
}
