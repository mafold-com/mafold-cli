//! `mafold connection` — your credentials at third parties, carried between
//! your own machines without ever being readable by Mafold.
//!
//! Link a provider once and every daemon you run can use it. The plaintext is
//! assembled here and nowhere else: the server sees a provider slug, a masked
//! label, and a blob. See `.docs/connections-v1.md`; the ciphers are in
//! `vault.rs`.
//!
//! The command surface follows the trust story rather than the CRUD:
//!
//!   list / add / show / rm      the credentials
//!   devices / approve / revoke  who is allowed to open them
//!   unlock / recovery           getting a key onto a machine
//!
//! Auth is the HUMAN session (`mafold login`), never a bot token — a daemon
//! running on your laptop must not be able to enumerate your credentials just
//! because it shares the filesystem.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use crate::session;
use crate::vault::{self, DeviceKey, Key};
use mafold_core::mafold_types::connections::{provider as provider_spec, ProviderKind, PROVIDERS};

#[derive(Subcommand)]
pub enum ConnectionCmd {
    /// List your connections.
    List,
    /// Link a credential: `mafold connection add anthropic --provider anthropic-api`.
    Add {
        /// A short name you'll refer to it by (`anthropic`, `claude-max`).
        name: String,
        /// Provider id — see `mafold connection providers`.
        #[arg(long)]
        provider: String,
        /// Import from the local file the provider's own CLI wrote
        /// (Claude Code / Codex OAuth). Beats pasting a refresh token by hand.
        #[arg(long)]
        import: bool,
        /// Read the value from the provider's conventional environment variable.
        #[arg(long)]
        from_env: bool,
        /// A human tag for the linked identity. Stored in CLEARTEXT so the list
        /// is readable; defaults to a masked tail of the secret.
        #[arg(long)]
        label: Option<String>,
    },
    /// Show one connection. Metadata only unless you ask for the secret.
    Show {
        name: String,
        /// Print the decrypted secret to stdout.
        #[arg(long)]
        reveal: bool,
    },
    /// Print `export VAR=…` lines for a connection, to feed a local tool.
    Env { name: String },
    /// What a connection can do — its methods, straight from the provider.
    Methods {
        name: String,
        /// Print each method's full JSON Schema rather than a one-line summary.
        #[arg(long)]
        schema: bool,
    },
    /// Call one method: `mafold connection call notion search --params '{"query":"roadmap"}'`.
    Call {
        name: String,
        /// Method name from `mafold connection methods <name>`.
        method: String,
        /// Arguments as a JSON object. Omit for methods that take none.
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Forget a connection (Mafold's copy only — never the provider's).
    Rm { name: String },
    /// The providers this build knows how to hold a credential for.
    Providers,
    /// Devices allowed to open your connections.
    Devices,
    /// Approve a pending device, wrapping the master key for it.
    Approve {
        /// Device id from `mafold connection devices`.
        device_id: String,
        /// Skip the fingerprint confirmation. Only for scripted enrollment of a
        /// machine you already control.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a device, and re-key so it genuinely loses access.
    Revoke {
        device_id: String,
        /// Skip the re-key. The removed machine keeps whatever it already
        /// holds — only use this if it was never approved.
        #[arg(long)]
        no_rotate: bool,
    },
    /// Fetch this machine's wrapped master key once another device approves it.
    Unlock,
    /// Stay online and answer connection calls addressed to your devices.
    ///
    /// While this runs, bots you've granted a connection to get their calls
    /// executed HERE — the credential is opened on this machine and only the
    /// result leaves it. Close it and another of your devices (an open web
    /// client, your phone) takes over; none online means calls fail with a
    /// message saying so.
    Listen,
    /// Set the offline recovery passphrase (wraps the master key under it).
    SetRecovery,
    /// Recover the master key on a machine with no approved device.
    Recover,
}

// ── plumbing ───────────────────────────────────────────────────────────────

/// A connection command always speaks as the person, so it builds its own
/// client from the saved human session instead of the ambient bot token.
fn human_client(base: &str) -> Result<(Client, session::Session)> {
    let sess = session::load()
        .context("not logged in — run `mafold login` first (connections belong to your account)")?;
    Ok((
        Client::new(base.to_string(), sess.token.clone()),
        sess.clone(),
    ))
}

fn as_array(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).and_then(|x| x.as_array()).cloned().unwrap_or_default()
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Mask everything but the last 4 — the same shape the api uses for bot
/// secrets, so a masked value looks the same wherever it appears.
fn mask_tail(v: &str) -> String {
    let n = v.chars().count();
    if n <= 4 {
        return "•".repeat(n);
    }
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{}{}", "•".repeat((n - 4).min(8)), tail)
}

/// Register this machine's public key and report where it stands.
///
/// Called by every path that needs a key, because a device that was revoked or
/// re-installed must re-enrol rather than fail with a decrypt error.
async fn register(client: &Client, sess: &session::Session, dev: &DeviceKey) -> Result<Value> {
    let reg = client
        .call(
            "registerVaultDevice",
            json!({
                "device_id": sess.device_id,
                "device_name": sess.device_name,
                "public_key": dev.public,
                "fingerprint": vault::fingerprint(&dev.public),
            }),
        )
        .await
        .context("registerVaultDevice failed")?;

    // Every command comes through here, so this is where "any machine of mine
    // that holds the key hands it to the others" actually becomes true. Hooking
    // it to the unlock path alone was too narrow: `list` and `devices` never
    // unlock, so a laptop could sit next to a waiting phone all day and do
    // nothing about it — which is exactly the stuck feeling the ceremony was
    // removed to fix.
    if let Some((key, key_id)) = vault::cached_umk(dev) {
        if !key_id.is_empty() && s(&reg["device"], "key_id") == key_id {
            auto_approve_pending(client, &key, &key_id).await;
        }
    }
    Ok(reg)
}

/// The master key for this machine, or a clear explanation of what to do next.
///
/// Order matters: the local cache first (a daemon must not do a round trip per
/// read), then the wrap left by an approving device. There is deliberately no
/// third fallback that mints a fresh key — that would silently orphan every
/// existing connection instead of saying "this device isn't approved yet".
/// Hand the key to every other machine of yours that is waiting for it.
///
/// **Owner ruling, 2026-08-12: signing in to your own client IS the
/// authorization.** No prompt, no fingerprint comparison, no command to run.
/// Whichever of your devices holds the key gives it to the others the moment it
/// notices, and the whole enrolment ceremony disappears.
///
/// The property that survives is the one worth having: the server still only
/// ever relays a wrap it has no key for, so it cannot read a credential, and
/// neither can anyone who steals its database or a session token. What is given
/// up is the defence against the SERVER itself substituting a device public key
/// — an active, targeted attack by us, traded away because the ceremony that
/// prevented it charged every honest user a step they frequently could not
/// perform at all.
async fn auto_approve_pending(client: &Client, umk: &Key, key_id: &str) {
    let Ok(v) = client.call("listVaultDevices", json!({})).await else {
        return;
    };
    for d in as_array(&v, "items") {
        let has_key = d["has_key"].as_bool().unwrap_or(false);
        let approved = d["approved"].as_bool().unwrap_or(false);
        if approved && has_key {
            continue;
        }
        let (id, public) = (s(&d, "device_id"), s(&d, "public_key"));
        if id.is_empty() || public.is_empty() {
            continue;
        }
        let Ok(wrapped) = vault::wrap_umk_for(&public, umk) else {
            continue;
        };
        // Best effort per device: one that fails is not a reason to abandon the
        // rest, and nothing here is worth interrupting the command the user
        // actually ran.
        let _ = client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": id,
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": public,
                }),
            )
            .await;
    }
}

async fn unlock(client: &Client, sess: &session::Session) -> Result<(Key, String, DeviceKey)> {
    let dev = vault::device_key()?;
    let reg = register(client, sess, &dev).await?;

    if let Some((key, key_id)) = vault::cached_umk(&dev) {
        // Trust the cache only when the server still records THIS generation
        // wrapped for THIS device. An empty id is not a permissive "server
        // doesn't track that" — it means no wrap is on record, i.e. the device
        // was revoked. Treating that as trustworthy would let a revoked machine
        // keep working off its cache until something happened to re-key.
        // `register` above has already passed the key on to anything waiting.
        if !key_id.is_empty() && s(&reg["device"], "key_id") == key_id {
            return Ok((key, key_id, dev));
        }
        vault::forget_cached_umk();
    }

    if reg["first"].as_bool() == Some(true) {
        // Nobody can approve us because nobody holds a key yet: this account's
        // vault starts here.
        let umk = Key::random();
        let key_id = vault::new_key_id();
        let wrapped = vault::wrap_umk_for(&dev.public, &umk)?;
        client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": sess.device_id,
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": dev.public,
                }),
            )
            .await
            .context("approveVaultDevice (self) failed")?;
        vault::cache_umk(&umk, &dev, &key_id)?;
        println!("✓ vault created on {} ({})", sess.device_name, vault::fingerprint(&dev.public));
        println!("  set an offline recovery passphrase now:  mafold connection set-recovery");
        return Ok((umk, key_id, dev));
    }

    match client
        .call("getVaultKey", json!({ "device_id": sess.device_id }))
        .await
    {
        Ok(v) => {
            let wrapped = s(&v, "sealed_umk");
            let key_id = s(&v, "key_id");
            let umk = vault::unwrap_umk(&dev.secret, &wrapped).map_err(|e| anyhow!("{e}"))?;
            vault::cache_umk(&umk, &dev, &key_id)?;
            auto_approve_pending(client, &umk, &key_id).await;
            Ok((umk, key_id, dev))
        }
        // Not "you forgot to approve it" — there is nothing to approve. Any
        // device of yours that is signed in hands this one the key by itself;
        // this message only appears when none of them has been online since
        // this machine registered, so the only accurate instruction is to open
        // Mafold somewhere and come back.
        Err(_) => bail!(
            "this machine doesn't have your vault key yet.\n\n  \
             open Mafold on a device that already has it (web, mac or iOS) and it \
             will hand the key over — then run this again.\n  \
             no other device? recover with your passphrase:  mafold connection recover"
        ),
    }
}

/// Open a connection's payload.
///
/// `key_id` is checked first so that the common failure — this device holds a
/// retired master key — reports itself as such. Letting it fall through to the
/// AEAD would surface "wrong key or corrupt data", which reads like the
/// credential was damaged rather than that the caller needs to re-unlock.
fn open_payload(umk: &Key, key_id: &str, conn: &Value) -> Result<serde_json::Map<String, Value>> {
    let want = s(conn, "key_id");
    if !want.is_empty() && !key_id.is_empty() && want != key_id {
        bail!(
            "`{}` was re-keyed (it needs master key {want}, this device holds {key_id}).\n  \
             run `mafold connection unlock` — or, if this device was revoked, re-approve it \
             from one that wasn't.",
            s(conn, "name")
        );
    }
    // The DEK dance lives in the shared core, so the cli and the browser seal
    // and open the same way by construction rather than by review.
    let plain = vault::open_payload(umk, &s(conn, "blob"), &s(conn, "wrapped_dek"))
        .map_err(|e| anyhow!("{e}"))?;
    serde_json::from_str(&plain).context("connection payload is not JSON")
}

/// Seal a payload under a fresh DEK wrapped by the master key.
fn seal_payload(umk: &Key, fields: &serde_json::Map<String, Value>) -> Result<(String, String)> {
    let sealed = vault::seal_payload(umk, &serde_json::to_string(fields)?);
    Ok((sealed.blob, sealed.wrapped_dek))
}

async fn fetch(client: &Client, name: &str) -> Result<Value> {
    let v = client.call("listConnections", json!({})).await?;
    as_array(&v, "items")
        .into_iter()
        .find(|c| s(c, "name") == name)
        .ok_or_else(|| anyhow!("no connection named `{name}` — see `mafold connection list`"))
}

// ── commands ───────────────────────────────────────────────────────────────

pub async fn run(base: &str, cmd: ConnectionCmd) -> Result<()> {
    let (client, sess) = human_client(base)?;
    match cmd {
        ConnectionCmd::Providers => {
            providers();
            Ok(())
        }
        ConnectionCmd::List => list(&client).await,
        ConnectionCmd::Add { name, provider, import, from_env, label } => {
            add(&client, &sess, &name, &provider, import, from_env, label).await
        }
        ConnectionCmd::Show { name, reveal } => show(&client, &sess, &name, reveal).await,
        ConnectionCmd::Env { name } => env(&client, &sess, &name).await,
        ConnectionCmd::Methods { name, schema } => {
            methods(base, &client, &sess, &name, schema).await
        }
        ConnectionCmd::Call { name, method, params } => {
            call(base, &client, &sess, &name, &method, &params).await
        }
        ConnectionCmd::Rm { name } => rm(&client, &name).await,
        ConnectionCmd::Devices => devices(&client, &sess).await,
        ConnectionCmd::Approve { device_id, yes } => approve(&client, &sess, &device_id, yes).await,
        ConnectionCmd::Revoke { device_id, no_rotate } => {
            revoke(&client, &sess, &device_id, no_rotate).await
        }
        ConnectionCmd::Listen => listen(base, &client, &sess).await,
        ConnectionCmd::Unlock => {
            let (_, key_id, dev) = unlock(&client, &sess).await?;
            println!(
                "✓ unlocked on {} — key {} · fingerprint {}",
                sess.device_name,
                key_id,
                vault::fingerprint(&dev.public)
            );
            Ok(())
        }
        ConnectionCmd::SetRecovery => set_recovery(&client, &sess).await,
        ConnectionCmd::Recover => recover(&client, &sess).await,
    }
}

fn providers() {
    println!("{:<20} {:<26} {:<8} {}", "ID", "PROVIDER", "AUTH", "LINK VIA");
    for p in PROVIDERS {
        let how = match (p.import_path, p.env_var) {
            (Some(path), _) => format!("--import  (~/{path})"),
            (None, Some(v)) => format!("--from-env  (${v})"),
            (None, None) => "paste".to_string(),
        };
        let kind = match p.kind {
            ProviderKind::OAuth => "oauth",
            ProviderKind::ApiKey => "key",
        };
        println!("{:<20} {:<26} {:<8} {}", p.id, p.display, kind, how);
    }
}

async fn list(client: &Client) -> Result<()> {
    let v = client.call("listConnections", json!({})).await?;
    let items = as_array(&v, "items");
    if items.is_empty() {
        println!("(no connections)\n\n  link one:  mafold connection add <name> --provider <id>");
        println!("  providers: mafold connection providers");
        return Ok(());
    }
    println!("{:<16} {:<20} {:<10} {}", "NAME", "PROVIDER", "STATUS", "LABEL");
    for c in items {
        let prov = s(&c, "provider");
        // An unknown provider is shown, not hidden: it means this cli is older
        // than the connection, and saying so beats an empty row.
        let status = if provider_spec(&prov).is_some() { "linked" } else { "unknown" };
        println!(
            "{:<16} {:<20} {:<10} {}",
            s(&c, "name"),
            prov,
            status,
            s(&c, "label")
        );
    }
    Ok(())
}

/// Collect a provider's fields, by import, environment, or prompt.
fn collect(spec: &mafold_core::mafold_types::connections::ProviderSpec, import: bool, from_env: bool)
    -> Result<serde_json::Map<String, Value>>
{
    let mut out = serde_json::Map::new();

    if import {
        let path = spec
            .import_path
            .ok_or_else(|| anyhow!("{} has no local credential file to import", spec.id))?;
        let full = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(path);
        let raw = std::fs::read_to_string(&full)
            .with_context(|| format!("read {} — log in with that tool first", full.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not JSON", full.display()))?;
        // Vendors nest their bag differently; search rather than hard-code a
        // path per vendor, so a layout change costs nothing here.
        for f in spec.fields {
            if let Some(v) = find_key(&parsed, f.key) {
                out.insert(f.key.to_string(), v);
            }
        }
        if out.is_empty() {
            bail!("found no recognizable fields in {}", full.display());
        }
    } else if from_env {
        let var = spec
            .env_var
            .ok_or_else(|| anyhow!("{} has no conventional environment variable", spec.id))?;
        let val = std::env::var(var)
            .with_context(|| format!("${var} is not set"))?;
        let first = spec.fields.first().ok_or_else(|| anyhow!("provider has no fields"))?;
        out.insert(first.key.to_string(), Value::String(val));
    } else {
        for f in spec.fields {
            let label = if f.required {
                format!("{}: ", f.label)
            } else {
                format!("{} (optional, blank to skip): ", f.label)
            };
            let val = crate::prompt_password(&label);
            if !val.is_empty() {
                out.insert(f.key.to_string(), Value::String(val));
            }
        }
    }

    for f in spec.fields {
        if f.required && !out.contains_key(f.key) {
            bail!("{} is required for {}", f.label, spec.id);
        }
    }
    Ok(out)
}

/// Depth-first search for a key anywhere in a vendor's JSON.
fn find_key(v: &Value, key: &str) -> Option<Value> {
    match v {
        Value::Object(m) => {
            if let Some(found) = m.get(key) {
                if !found.is_null() && !found.is_object() && !found.is_array() {
                    return Some(found.clone());
                }
            }
            m.values().find_map(|x| find_key(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    client: &Client,
    sess: &session::Session,
    name: &str,
    provider: &str,
    import: bool,
    from_env: bool,
    label: Option<String>,
) -> Result<()> {
    let spec = provider_spec(provider).ok_or_else(|| {
        anyhow!(
            "unknown provider `{provider}` — see `mafold connection providers`"
        )
    })?;
    let fields = collect(spec, import, from_env)?;
    let (umk, key_id, _) = unlock(client, sess).await?;
    let (blob, wrapped_dek) = seal_payload(&umk, &fields)?;

    // The label is the one thing we hand over in the clear, so derive it from
    // the least sensitive part available and say what it is.
    let label = label.unwrap_or_else(|| {
        fields
            .get("account")
            .or_else(|| fields.get("account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let primary = spec.fields.first().map(|f| f.key).unwrap_or("api_key");
                fields
                    .get(primary)
                    .and_then(|v| v.as_str())
                    .map(mask_tail)
                    .unwrap_or_default()
            })
    });

    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": label,
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id,
            }),
        )
        .await
        .context("putConnection failed")?;
    println!("✓ linked {name} → {} ({label})", spec.display);
    println!("  the server stored ciphertext it cannot open; only your enrolled devices can.");
    if spec.mcp_url.is_some() {
        println!("  its methods:  mafold connection methods {name}");
    }
    Ok(())
}

async fn show(client: &Client, sess: &session::Session, name: &str, reveal: bool) -> Result<()> {
    let conn = fetch(client, name).await?;
    println!("name      {}", s(&conn, "name"));
    println!("provider  {}", s(&conn, "provider"));
    println!("label     {}", s(&conn, "label"));
    println!("key       {}", s(&conn, "key_id"));
    if !reveal {
        println!("\n(secret withheld — `mafold connection show {name} --reveal` to decrypt here)");
        return Ok(());
    }
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = open_payload(&umk, &key_id, &conn)?;
    println!();
    for (k, v) in fields {
        // Vendors store expiries as numbers and flags as bools; printing only
        // strings would render those blank and read as a missing field.
        let shown = match &v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        println!("{k:<16}{shown}");
    }
    Ok(())
}

/// Shell exports for a connection, so a local tool can consume it without a
/// bespoke integration. Deliberately not written to any file: piping into
/// `eval` keeps the plaintext in a process, not on disk.
async fn env(client: &Client, sess: &session::Session, name: &str) -> Result<()> {
    let conn = fetch(client, name).await?;
    let provider = s(&conn, "provider");
    let spec = provider_spec(&provider)
        .ok_or_else(|| anyhow!("`{provider}` is unknown to this cli — run `mafold update`"))?;
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = open_payload(&umk, &key_id, &conn)?;
    let var = spec
        .env_var
        .ok_or_else(|| anyhow!("{} is an OAuth bag, not a single env var — use `show --reveal`", spec.id))?;
    let primary = spec.fields.first().map(|f| f.key).unwrap_or("api_key");
    let val = fields
        .get(primary)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("connection has no `{primary}`"))?;
    println!("export {var}={val}");
    Ok(())
}

/// A device-side runtime, unlocked.
///
/// The master key is passed in rather than reachable from the runtime, so the
/// core never learns how this machine stores its device key — that stays here,
/// where the 0600 file and the platform's rules live.
async fn runtime(
    base: &str,
    client: &Client,
    sess: &session::Session,
) -> Result<mafold_core::connections::Runtime> {
    let (umk, _, _) = unlock(client, sess).await?;
    // `base` here is the ORIGIN — `Client` appends `/api` itself, and the core's
    // rpc does not. Passing it through unchanged makes every core call a 404
    // that reads as "this server is older than this client", which is a very
    // convincing wrong answer.
    Ok(mafold_core::connections::Runtime::new(
        &format!("{base}/api"),
        &sess.token,
        umk,
    ))
}

/// Hold a human WS open and let the CORE answer connection calls on it.
///
/// This is the terminal's version of what the web client does passively: the
/// frame arrives, `connections::handle_event` decides whether it's ours, claims
/// it, opens the vault, calls the provider, answers. Nothing here inspects the
/// event beyond handing it over — the whole point is that every device answers
/// with the same Rust.
async fn listen(base: &str, client: &Client, sess: &session::Session) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let mut rt = runtime(base, client, sess).await?;
    println!(
        "✓ listening as @{} on {} — connection calls granted to your bots run here.\n  ctrl-c to stop.",
        sess.username, sess.device_name
    );
    loop {
        let mut ws = match client.ws_connect().await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("ws connect failed ({e}) — retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };
        while let Some(frame) = ws.next().await {
            match frame {
                Ok(WsMsg::Text(t)) => {
                    if mafold_core::connections::handle_event(&mut rt, &t).await {
                        println!("· answered a connection call");
                    }
                }
                // The server pings every 25s and treats silence as death; an
                // unanswered ping here would look like "listen is on but calls
                // time out", which is the worst version of off.
                Ok(WsMsg::Ping(p)) => {
                    let _ = ws.send(WsMsg::Pong(p)).await;
                }
                Ok(WsMsg::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        eprintln!("ws dropped — reconnecting in 2s");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// What a connection can do, asked of the provider itself.
///
/// Nothing here is a Mafold-maintained list: the catalog comes from the
/// provider's MCP server, so a tool it ships tomorrow shows up without a
/// release. That is the reason this layer speaks MCP at all.
async fn methods(
    base: &str,
    client: &Client,
    sess: &session::Session,
    name: &str,
    schema: bool,
) -> Result<()> {
    let mut rt = runtime(base, client, sess).await?;
    let methods = rt.methods(name).await.map_err(|e| anyhow!("{e}"))?;
    if methods.is_empty() {
        println!("{name} offers no methods.");
        return Ok(());
    }
    println!("{} method{}", methods.len(), if methods.len() == 1 { "" } else { "s" });
    for m in &methods {
        let mark = if m.read_only { " " } else { "!" };
        println!("\n{mark} {}", m.name);
        if !m.description.is_empty() {
            // One line: a catalog is for choosing, and some descriptions run
            // to paragraphs.
            let first = m.description.lines().next().unwrap_or("");
            println!("    {}", first.chars().take(140).collect::<String>());
        }
        if schema {
            println!(
                "    {}",
                serde_json::to_string_pretty(&m.input_schema)
                    .unwrap_or_default()
                    .replace('\n', "\n    ")
            );
        } else if let Some(props) = m.input_schema.get("properties").and_then(|p| p.as_object()) {
            let required: Vec<&str> = m
                .input_schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let args: Vec<String> = props
                .keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("[{k}]")
                    }
                })
                .collect();
            if !args.is_empty() {
                println!("    params: {}", args.join(" "));
            }
        }
    }
    println!("\n! = may write. --schema for full parameter types.");
    Ok(())
}

/// Run one method and print what came back.
async fn call(
    base: &str,
    client: &Client,
    sess: &session::Session,
    name: &str,
    method: &str,
    params: &str,
) -> Result<()> {
    // Parse before unlocking: a typo in `--params` should not cost a vault
    // round trip, and the error should point at the JSON rather than arriving
    // after something that looks like real work.
    let args: Value = serde_json::from_str(params)
        .with_context(|| format!("--params is not JSON: {params}"))?;
    if !args.is_object() {
        bail!("--params must be a JSON object, e.g. --params '{{\"query\":\"roadmap\"}}'");
    }
    let mut rt = runtime(base, client, sess).await?;
    let out = rt.call(name, method, args).await.map_err(|e| anyhow!("{e}"))?;

    // MCP wraps results in `content: [{type, text}]`. Unwrap the common
    // all-text case so a shell pipeline gets the payload rather than the
    // envelope; anything richer prints whole.
    if let Some(items) = out.get("content").and_then(|c| c.as_array()) {
        let all_text = !items.is_empty()
            && items
                .iter()
                .all(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"));
        if all_text {
            for i in items {
                println!("{}", i.get("text").and_then(|t| t.as_str()).unwrap_or(""));
            }
            // A tool that reports failure in-band still exits non-zero, or a
            // script would treat "I couldn't do that" as success.
            if out.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                bail!("{name}.{method} reported an error");
            }
            return Ok(());
        }
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn rm(client: &Client, name: &str) -> Result<()> {
    client
        .call("deleteConnection", json!({ "name": name }))
        .await
        .context("deleteConnection failed")?;
    println!("✓ removed {name} from Mafold.");
    println!("  the credential still exists at the provider — revoke it there if you meant to.");
    Ok(())
}

async fn devices(client: &Client, sess: &session::Session) -> Result<()> {
    let dev = vault::device_key()?;
    register(client, sess, &dev).await?;
    let v = client.call("listVaultDevices", json!({})).await?;
    let items = as_array(&v, "items");
    let items_pending = items.iter().any(|d| {
        d["approved"].as_bool() != Some(true) || d["has_key"].as_bool() != Some(true)
    });
    println!("{:<20} {:<22} {:<12} {}", "DEVICE", "NAME", "STATUS", "FINGERPRINT");
    for d in items {
        let id = s(&d, "device_id");
        let approved = d["approved"].as_bool() == Some(true);
        let has_key = d["has_key"].as_bool() == Some(true);
        let mine = id == sess.device_id;
        let status = match (approved && has_key, mine) {
            (true, true) => "this device",
            (true, false) => "enrolled",
            (false, true) => "pending (you)",
            (false, false) => "pending",
        };
        println!("{:<20} {:<22} {:<12} {}", id, s(&d, "device_name"), status, s(&d, "fingerprint"));
    }
    // No "approve a pending device" footer. `register` above already handed the
    // key to everything waiting, so anything still listed as pending is waiting
    // on a machine that hasn't been online — not on the user. Advertising a
    // command here would suggest otherwise.
    if items_pending {
        println!("\npending devices get the key automatically from any machine of yours that has it.");
    }
    Ok(())
}

async fn approve(
    client: &Client,
    sess: &session::Session,
    device_id: &str,
    yes: bool,
) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let v = client.call("listVaultDevices", json!({})).await?;
    let target = as_array(&v, "items")
        .into_iter()
        .find(|d| s(d, "device_id") == device_id)
        .ok_or_else(|| anyhow!("no device `{device_id}` — see `mafold connection devices`"))?;

    let public_key = s(&target, "public_key");
    let fp = vault::fingerprint(&public_key);
    println!("device      {}", s(&target, "device_id"));
    println!("name        {}", s(&target, "device_name"));
    println!("fingerprint {fp}");

    // The server relays the public key, so the server could substitute one. The
    // human comparing this string to the one printed on the other machine is
    // the actual authorization — everything else here is bookkeeping.
    if !yes {
        let ans = crate::prompt("\nDoes that fingerprint match the other machine? [y/N] ");
        if !ans.eq_ignore_ascii_case("y") {
            println!("aborted — nothing was shared.");
            return Ok(());
        }
    }

    let wrapped = vault::wrap_umk_for(&public_key, &umk)?;
    client
        .call(
            "approveVaultDevice",
            json!({
                "device_id": device_id,
                "sealed_umk": wrapped,
                "key_id": key_id,
                "public_key": public_key,
            }),
        )
        .await
        .context("approveVaultDevice failed")?;
    println!("✓ approved — run `mafold connection unlock` on that machine.");
    Ok(())
}

async fn revoke(
    client: &Client,
    sess: &session::Session,
    device_id: &str,
    no_rotate: bool,
) -> Result<()> {
    if device_id == sess.device_id {
        bail!("that's this machine — revoke it from another device, or you'll lock yourself out");
    }
    client
        .call("revokeVaultDevice", json!({ "device_id": device_id }))
        .await
        .context("revokeVaultDevice failed")?;
    println!("✓ {device_id} removed from the vault.");

    if no_rotate {
        println!("  NOT re-keyed: if that machine was ever approved, it still holds the master key");
        println!("  and can still open every connection. Re-key with `mafold connection revoke … `");
        return Ok(());
    }
    rotate(client, sess).await
}

/// Mint a new master key, re-seal every connection under it, and re-wrap it for
/// the devices that remain.
///
/// This is what makes revocation real. Deleting a row cannot reach into a
/// machine that already copied the key, so the only honest revocation is to
/// stop using the key it has.
async fn rotate(client: &Client, sess: &session::Session) -> Result<()> {
    let (old_umk, old_key_id, dev) = unlock(client, sess).await?;
    let conns = as_array(
        &client.call("listConnections", json!({})).await?,
        "items",
    );
    let devices = as_array(
        &client.call("listVaultDevices", json!({})).await?,
        "items",
    );

    let new_umk = Key::random();
    let key_id = vault::new_key_id();

    // Re-seal first. If this fails halfway, the old key still opens everything
    // and the vault is merely un-rotated — whereas re-wrapping keys first would
    // leave devices holding a key that opens nothing.
    for c in &conns {
        let name = s(c, "name");
        let fields = open_payload(&old_umk, &old_key_id, c)
            .with_context(|| format!("re-key {name}: could not open it with the current key"))?;
        let (blob, wrapped_dek) = seal_payload(&new_umk, &fields)?;
        client
            .call(
                "putConnection",
                json!({
                    "name": name,
                    "provider": s(c, "provider"),
                    "label": s(c, "label"),
                    "blob": blob,
                    "wrapped_dek": wrapped_dek,
                    "key_id": key_id,
                }),
            )
            .await
            .with_context(|| format!("re-key {name}: putConnection failed"))?;
    }

    for d in &devices {
        if d["approved"].as_bool() != Some(true) {
            continue;
        }
        let public_key = s(d, "public_key");
        let wrapped = vault::wrap_umk_for(&public_key, &new_umk)?;
        client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": s(d, "device_id"),
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": public_key,
                }),
            )
            .await
            .with_context(|| format!("re-wrap for {}", s(d, "device_name")))?;
    }

    vault::cache_umk(&new_umk, &dev, &key_id)?;
    println!(
        "✓ re-keyed {} connection(s) under a new master key; {} device(s) re-wrapped.",
        conns.len(),
        devices.iter().filter(|d| d["approved"].as_bool() == Some(true)).count()
    );
    println!("  set the recovery passphrase again — the old one wraps the retired key:");
    println!("    mafold connection set-recovery");
    Ok(())
}

async fn set_recovery(client: &Client, sess: &session::Session) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let pass = crate::prompt_password("Recovery passphrase: ");
    if pass.chars().count() < 12 {
        bail!("use at least 12 characters — this is the one thing an attacker can grind offline");
    }
    let again = crate::prompt_password("Again: ");
    if pass != again {
        bail!("they don't match");
    }
    let blob = vault::wrap_umk_with_passphrase(&umk, &pass)?;
    client
        .call(
            "putVaultRecovery",
            json!({
                "salt": blob.salt,
                "mem_kib": blob.mem_kib,
                "time_cost": blob.time_cost,
                "lanes": blob.lanes,
                "sealed_umk": blob.sealed_umk,
                "key_id": key_id,
            }),
        )
        .await
        .context("putVaultRecovery failed")?;
    println!("✓ recovery set. Write the passphrase down somewhere physical —");
    println!("  we cannot reset it, which is the same reason we cannot read your connections.");
    Ok(())
}

async fn recover(client: &Client, sess: &session::Session) -> Result<()> {
    let dev = vault::device_key()?;
    register(client, sess, &dev).await?;
    let v = client
        .call("getVaultRecovery", json!({}))
        .await
        .context("no recovery blob is set for this account")?;
    let r = &v["recovery"];
    let blob = vault::RecoveryBlob {
        salt: s(r, "salt"),
        mem_kib: r["mem_kib"].as_u64().unwrap_or(0) as u32,
        time_cost: r["time_cost"].as_u64().unwrap_or(0) as u32,
        lanes: r["lanes"].as_u64().unwrap_or(0) as u32,
        sealed_umk: s(r, "sealed_umk"),
    };
    let pass = crate::prompt_password("Recovery passphrase: ");
    let umk = vault::unwrap_umk_with_passphrase(&blob, &pass)?;
    let key_id = s(r, "key_id");

    // Recovering proves possession of the passphrase, not of an approved
    // device — so enrol this machine properly rather than leaving it working
    // off a cache that `devices` would never list.
    let wrapped = vault::wrap_umk_for(&dev.public, &umk)?;
    client
        .call(
            "approveVaultDevice",
            json!({
                "device_id": sess.device_id,
                "sealed_umk": wrapped,
                "key_id": key_id,
                "public_key": dev.public,
            }),
        )
        .await
        .context("enrolling this device after recovery failed")?;
    vault::cache_umk(&umk, &dev, &key_id)?;
    println!("✓ recovered and enrolled {} ({})", sess.device_name, vault::fingerprint(&dev.public));
    println!("  review your devices and revoke anything you don't recognize:");
    println!("    mafold connection devices");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_keeps_only_the_tail() {
        assert_eq!(mask_tail("sk-ant-api03-abcd3f9a"), "••••••••3f9a");
        assert_eq!(mask_tail("ab"), "••");
    }

    /// Vendors nest their token bags differently and change the nesting between
    /// versions; import searches instead of hard-coding a path per vendor.
    #[test]
    fn import_finds_fields_at_any_depth() {
        let v: Value = serde_json::from_str(
            r#"{"claudeAiOauth":{"accessToken":"x","access_token":"tok","expires_at":123}}"#,
        )
        .unwrap();
        assert_eq!(find_key(&v, "access_token"), Some(Value::String("tok".into())));
        assert_eq!(find_key(&v, "expires_at"), Some(Value::Number(123.into())));
        assert_eq!(find_key(&v, "refresh_token"), None);
    }

    /// A container must never be mistaken for a value — that would store `{…}`
    /// as if it were a token and fail much later, at the third party.
    #[test]
    fn import_ignores_containers_with_the_right_name() {
        let v: Value = serde_json::from_str(r#"{"token":{"inner":"x"},"a":{"token":"real"}}"#).unwrap();
        assert_eq!(find_key(&v, "token"), Some(Value::String("real".into())));
    }

    #[test]
    fn every_registry_provider_is_addable() {
        for p in PROVIDERS {
            assert!(provider_spec(p.id).is_some());
        }
    }
}
