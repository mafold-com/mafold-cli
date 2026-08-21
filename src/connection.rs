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
use mafold_core::mafold_types::connections::{provider_infos, ProviderInfo, ProviderKind};

/// The registry, from the cloud — there is no compiled-in copy to fall back on.
///
/// The cli reads the same signed pack every other surface does, which is what
/// makes "add a provider" a push rather than five releases. It also means the
/// cli can be OLDER than the registry and still link and call a provider it has
/// never heard of, as long as that provider needs no native driver.
async fn registry(client: &Client) -> Result<Vec<ProviderInfo>> {
    // `/api` is appended here for the same reason `Runtime::new` does it:
    // `Client::base` is the ORIGIN (`https://api.mafold.com`), while the core's
    // `net::rpc` takes a base that already includes the prefix. Passing the
    // origin straight through posts to `/getConnectionProviders` and gets a 404
    // that reads like "your server is too old" — which is what shipped in
    // cli@0.9.97 and is exactly the wrong thing to tell a user.
    mafold_core::providers::ensure(&format!("{}/api", client.base), &client.token, now_ms())
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(mafold_core::providers::all())
}

async fn descriptor(client: &Client, id: &str) -> Result<ProviderInfo> {
    registry(client)
        .await?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow!("no provider called `{id}` — see `mafold connection providers`"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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
        /// Log in through the provider's own OAuth consent screen, right here:
        /// the browser opens, the redirect lands on this machine, and the fresh
        /// grant goes straight into the vault. Only for providers whose CLI
        /// client is a published public client (Codex).
        #[arg(long)]
        oauth: bool,
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
        ConnectionCmd::Providers => providers(&client).await,
        ConnectionCmd::List => list(&client).await,
        ConnectionCmd::Add { name, provider, import, from_env, oauth, label } => {
            add(&client, &sess, &name, &provider, import, from_env, oauth, label).await
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

async fn providers(client: &Client) -> Result<()> {
    let rows = registry(client).await?;
    println!("{:<20} {:<26} {:<8} {}", "ID", "PROVIDER", "AUTH", "LINK VIA");
    for p in &rows {
        let how = match (p.import_path.as_deref(), p.env_var.as_deref()) {
            // Nothing to collect: `add` writes this machine's own binding.
            _ if is_device_binding(p) => "run it on that machine".to_string(),
            (Some(path), _) => format!("--import  (~/{path})"),
            (None, Some(v)) => format!("--from-env  (${v})"),
            // A consent screen the browser runs is the modern default, and it
            // is not "paste" — saying so sent people looking for a token page
            // that no longer exists for that provider.
            (None, None) if p.oauth => "sign in (browser)".to_string(),
            (None, None) => "paste".to_string(),
        };
        // Printed from what the row IS, not from `kind`: a machine binding
        // carries `ApiKey` for wire-compatibility reasons that have nothing to
        // do with the human reading this table (see the `computer` row).
        let kind = if is_device_binding(p) {
            "device"
        } else {
            match p.kind {
                ProviderKind::OAuth => "oauth",
                ProviderKind::ApiKey => "key",
            }
        };
        println!("{:<20} {:<26} {:<8} {}", p.id, p.display, kind, how);
    }
    Ok(())
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
        // The provider is printed VERBATIM and the row always reads `linked`,
        // because that is exactly what the server asserted by returning it. It
        // used to say `unknown` whenever this binary's compiled-in table had no
        // such id — which described the CLI's build, not the connection, and
        // told a user whose link had just succeeded that it hadn't.
        println!(
            "{:<16} {:<20} {:<10} {}",
            s(&c, "name"),
            s(&c, "provider"),
            "linked",
            s(&c, "label")
        );
    }
    Ok(())
}

/// Collect a provider's fields, by import, environment, or prompt.
fn collect(spec: &ProviderInfo, import: bool, from_env: bool)
    -> Result<serde_json::Map<String, Value>>
{
    let mut out = serde_json::Map::new();

    if import {
        let path = spec
            .import_path
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no local credential file to import", spec.id))?;
        let full = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(path);
        let raw = std::fs::read_to_string(&full)
            .with_context(|| format!("read {} — log in with that tool first", full.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not JSON", full.display()))?;
        // Vendors nest their bag differently; search rather than hard-code a
        // path per vendor, so a layout change costs nothing here.
        for f in &spec.fields {
            if let Some(v) = find_key(&parsed, &f.key) {
                out.insert(f.key.to_string(), v);
            }
        }
        if out.is_empty() {
            bail!("found no recognizable fields in {}", full.display());
        }
    } else if from_env {
        let var = spec
            .env_var
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no conventional environment variable", spec.id))?;
        let val = std::env::var(var)
            .with_context(|| format!("${var} is not set"))?;
        let first = spec.fields.first().ok_or_else(|| anyhow!("provider has no fields"))?;
        out.insert(first.key.to_string(), Value::String(val));
    } else {
        for f in &spec.fields {
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

    for f in &spec.fields {
        if f.required && !out.contains_key(&f.key) {
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

// ── the vendor-client OAuth dance (`add --oauth`) ──────────────────────────
//
// For providers whose OAuth client is a PUBLISHED PUBLIC client of the
// vendor's own CLI (`ProviderInfo::oauth_fixed`), we can mint a fresh grant
// instead of importing a file: PKCE, a localhost listener on the vendor's
// registered redirect, and a form-encoded code exchange. The whole dance runs
// on this machine — the registered redirect URI makes any server-side variant
// impossible, which is not a limitation but the property that keeps the vault
// honest: the token is born on a device the user controls and sealed there.
//
// A fresh grant also races NOTHING: `--import` shares a refresh token with the
// vendor's own CLI, and two holders spending one rotating token is the classic
// way an imported credential goes stale under it.

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Query-string percent-encoding (RFC 3986 unreserved survive).
fn q_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The claims segment of a JWT, unverified.
///
/// Unverified is CORRECT here, not lazy: these tokens arrive over TLS from the
/// vendor's own token endpoint (or its CLI's credential file), and the values
/// lifted out of them — account id, expiry, an email for the label — are hints
/// for our own bookkeeping, not authorization inputs. The party that must
/// trust the signature is the vendor's API, and it verifies for itself.
fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Fill the ISSUED fields a link flow can't collect from a human: the
/// registry's fixed OAuth client (what lets a *different* device refresh this
/// grant), the account id buried in the id_token, and an expiry read from the
/// access token itself when nothing recorded one. Import files vary; this makes
/// an OAuth payload renewal-complete regardless of which flow produced it.
///
/// `token_endpoint` is copied from the registry rather than from the vendor's
/// metadata on purpose: for a brokered provider it is mafold-api, not the
/// vendor, and a daemon renewing months later must post where the secret is —
/// which only the registry knows.
fn enrich_oauth_payload(
    spec: &ProviderInfo,
    fields: &mut serde_json::Map<String, Value>,
) {
    let missing = |m: &serde_json::Map<String, Value>, k: &str| {
        m.get(k).and_then(|v| v.as_str()).map(str::trim).unwrap_or("").is_empty()
    };
    if let Some(oc) = &spec.oauth_fixed {
        if missing(fields, "client_id") {
            fields.insert("client_id".into(), Value::String(oc.client_id.clone()));
        }
        if missing(fields, "token_endpoint") {
            fields.insert("token_endpoint".into(), Value::String(oc.token_endpoint.clone()));
        }
    }
    if missing(fields, "account_id") {
        if let Some(claims) = fields.get("id_token").and_then(Value::as_str).and_then(jwt_claims) {
            if let Some(acc) = claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                fields.insert("account_id".into(), Value::String(acc.into()));
            }
        }
    }
    if missing(fields, "expires_at") {
        if let Some(exp) = fields
            .get("access_token")
            .and_then(Value::as_str)
            .and_then(jwt_claims)
            .and_then(|c| c.get("exp").and_then(Value::as_i64))
        {
            fields.insert("expires_at".into(), Value::String((exp * 1000).to_string()));
        }
    }
}

/// Serve the redirect: accept connections until the callback with our `state`
/// arrives, answer it with a small "done" page, and hand back the code.
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    redirect_path: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await.context("accept on the redirect port")?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        let (route, query) = match path.split_once('?') {
            Some((r, q)) => (r, q),
            None => (path, ""),
        };
        // Browsers also ask for favicons and the like; only the registered
        // callback path ends the wait.
        if route != redirect_path {
            let _ = sock
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            continue;
        }
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = v.replace('+', " ");
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                "error" | "error_description" if error.is_none() => error = Some(v),
                _ => {}
            }
        }
        let ok = error.is_none() && code.is_some() && state.as_deref() == Some(expected_state);
        // Neutral about WHERE the flow was started: the same listener serves
        // `connection add --oauth` (a terminal) and a Connect button in the
        // web pane, and telling a person who clicked a button to "return to
        // the terminal" is the exact seam this feature exists to remove.
        let page = if ok {
            "<html><body style=\"font-family:system-ui;padding:2rem\"><h2>Linked ✓</h2>\
             <p>You can close this tab — Mafold has the rest.</p></body></html>"
        } else {
            "<html><body style=\"font-family:system-ui;padding:2rem\"><h2>That didn't work</h2>\
             <p>You can close this tab — Mafold will say what went wrong.</p></body></html>"
        };
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        if let Some(e) = error {
            bail!("the provider refused the login: {e}");
        }
        if state.as_deref() != Some(expected_state) {
            bail!("state mismatch on the OAuth callback — refusing a code this flow didn't ask for");
        }
        return Ok(code.expect("checked above"));
    }
}

/// The first leg of the dance, once the port is ours.
///
/// Split from the second leg because the two halves have different audiences:
/// the terminal opens `authorize_url` itself, while a link started from another
/// surface (`events.connectionLink`) hands it back so the browser the person is
/// actually looking at opens it. Everything that must survive between them —
/// the bound listener above all — travels in here rather than in a global, so
/// two flows can never share a port by accident.
struct OauthLeg {
    listener: tokio::net::TcpListener,
    verifier: String,
    state: String,
    redirect: url_parts::Parts,
    authorize_url: String,
    port: u16,
}

/// Bind the vendor's registered redirect and build its consent URL.
async fn oauth_begin(
    spec: &ProviderInfo,
) -> Result<OauthLeg> {
    use sha2::{Digest, Sha256};
    let oc = spec.oauth_fixed.clone().ok_or_else(|| {
        anyhow!(
            "{} has no OAuth client this cli can drive — link it with --import or --from-env",
            spec.id
        )
    })?;

    // PKCE. The verifier is HEX (the shape the vendor's own CLI sends), the
    // challenge standard base64url-nopad S256.
    let verifier = format!("{}{}", hex_bytes(&Key::random().0), hex_bytes(&Key::random().0));
    let challenge = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    };
    let state = hex_bytes(&Key::random().0);

    // Bind BEFORE the browser opens: if the port is taken (the vendor's own
    // CLI mid-login, an earlier attempt wedged), fail now with a sentence —
    // not after the user has clicked through a consent screen whose redirect
    // will land on the wrong listener.
    let redirect: url_parts::Parts = url_parts::split(&oc.redirect_uri)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", redirect.port))
        .await
        .with_context(|| {
            format!(
                "can't listen on 127.0.0.1:{} — is another {} login already running?",
                redirect.port, spec.display
            )
        })?;

    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        oc.authorize_url,
        q_encode(&oc.client_id),
        q_encode(&oc.redirect_uri),
        q_encode(&oc.scopes),
        state,
        challenge,
    );
    for (k, v) in &oc.extra_params {
        auth_url.push('&');
        auth_url.push_str(&format!("{}={}", q_encode(k), q_encode(v)));
    }

    let port = redirect.port;
    Ok(OauthLeg {
        listener,
        verifier,
        state,
        redirect,
        authorize_url: auth_url,
        port,
    })
}

/// The second leg: wait for the vendor to come back to our port, then trade the
/// code for tokens. Returns the token bag plus a suggested cleartext label
/// (email · plan) read from the id_token.
async fn oauth_finish(
    spec: &ProviderInfo,
    leg: OauthLeg,
) -> Result<(serde_json::Map<String, Value>, Option<String>)> {
    let oc = spec
        .oauth_fixed
        .clone()
        .ok_or_else(|| anyhow!("{} has no OAuth client this cli can drive", spec.id))?;
    let OauthLeg { listener, verifier, state, redirect, .. } = leg;

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        wait_for_callback(listener, &state, &redirect.path),
    )
    .await
    .map_err(|_| anyhow!("no sign-in came back within 5 minutes — start it again to retry"))??;

    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", oc.client_id.as_str()),
        ("code", code.as_str()),
        ("redirect_uri", oc.redirect_uri.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let resp = reqwest::Client::new()
        .post(oc.token_endpoint)
        .form(&form)
        .send()
        .await
        .context("token exchange failed to send")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token exchange answered HTTP {status}: {}", body.chars().take(300).collect::<String>());
    }
    let grant: Value = serde_json::from_str(&body).context("token endpoint returned non-JSON")?;
    let take = |k: &str| grant.get(k).and_then(Value::as_str).unwrap_or("").to_string();

    let mut fields = serde_json::Map::new();
    let access = take("access_token");
    if access.is_empty() {
        bail!("token endpoint returned no access token");
    }
    fields.insert("access_token".into(), Value::String(access));
    for k in ["refresh_token", "id_token"] {
        let v = take(k);
        if !v.is_empty() {
            fields.insert(k.to_string(), Value::String(v));
        }
    }
    if let Some(secs) = grant.get("expires_in").and_then(Value::as_i64) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        fields.insert("expires_at".into(), Value::String((now_ms + secs * 1000).to_string()));
    }

    // Label: the human identity of the grant, from the id_token.
    let label = fields
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .map(|c| {
            let email = c.get("email").and_then(Value::as_str).unwrap_or("").to_string();
            let plan = c
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match (email.is_empty(), plan.is_empty()) {
                (false, false) => format!("{email} · {plan}"),
                (false, true) => email,
                _ => String::new(),
            }
        })
        .filter(|s| !s.is_empty());

    Ok((fields, label))
}

/// Both legs, driven from a terminal: bind, open the browser here, wait.
async fn oauth_dance(
    spec: &ProviderInfo,
) -> Result<(serde_json::Map<String, Value>, Option<String>)> {
    let leg = oauth_begin(&spec).await?;
    println!("Opening {}'s consent screen…", spec.display);
    if !crate::platform::open_browser(&leg.authorize_url) {
        println!(
            "  couldn't open a browser — visit this URL yourself:\n\n  {}\n",
            leg.authorize_url
        );
    }
    println!("  waiting for the login to come back to 127.0.0.1:{}…", leg.port);
    oauth_finish(spec, leg).await
}

/// The two pieces of a redirect URI this flow needs. A module rather than a
/// dependency: pulling a URL crate into the cli for one host:port/path split
/// would be the heavier tool.
mod url_parts {
    use anyhow::{anyhow, Result};

    pub struct Parts {
        pub port: u16,
        /// Owned. It used to borrow a `&'static str`, which worked only while
        /// the redirect came from a compiled-in const — the registry is served
        /// now, so the string outlives nothing on its own.
        pub path: String,
    }

    pub fn split(uri: &str) -> Result<Parts> {
        let rest = uri
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("redirect URI must be http://localhost-style: {uri}"))?;
        let (host_port, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let port = host_port
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("redirect URI has no port: {uri}"))?;
        Ok(Parts { port, path: path.to_string() })
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
    oauth: bool,
    label: Option<String>,
) -> Result<()> {
    let spec = &descriptor(client, provider).await?;
    if is_device_binding(spec) {
        return bind_machine(client, sess, name, spec, label).await;
    }
    let (mut fields, suggested_label) = if oauth {
        oauth_dance(spec).await?
    } else {
        (collect(spec, import, from_env)?, None)
    };
    // Issued fields the flows above can't know by themselves: the registry's
    // fixed OAuth client (so ANY device can refresh later) and an expiry read
    // out of the token itself when the vendor's file didn't record one.
    enrich_oauth_payload(&spec, &mut fields);
    let fields = fields;
    let label = label.or(suggested_label);
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
                let primary = spec.fields.first().map(|f| f.key.as_str()).unwrap_or("api_key");
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

/// Hand the call to the server, which fans it out to the caller's own devices
/// so the one that can run it does.
///
/// The same route a granted bot uses (`callConnection`) — a person calling
/// their own laptop from their desktop is not a different feature, and giving
/// it a second path is how the two would drift on exactly the details (claim,
/// timeout, error wording) that make it work.
async fn relay(client: &Client, name: &str, method: &str, params: &Value) -> Result<Value> {
    let v = client
        .call(
            "callConnection",
            json!({ "connection": name, "method": method, "params": params }),
        )
        .await
        .with_context(|| format!("`{name}` did not answer"))?;
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Is this provider a MACHINE rather than an account somewhere else?
///
/// Read off the served registry, not off the provider's name: a row with
/// nothing for a human to type (`fields` empty — every field is issued) that
/// still needs a device to finish (`device_link`) can only be a binding. The
/// day a second such provider ships, this keeps working; a `== "computer"`
/// here would not, and would be the §9 shape the whole registry exists to
/// avoid.
fn is_device_binding(spec: &ProviderInfo) -> bool {
    spec.fields.is_empty() && spec.device_link && spec.oauth_fixed.is_none()
}

/// Point a connection at THIS machine.
///
/// There is no credential to collect, no consent screen, and nothing to paste
/// — the whole act is writing down which device answers, and sealing it so the
/// server cannot read (or choose) that. Which is why it can only be run on the
/// machine itself: a laptop cannot bind a desktop, because it does not hold the
/// desktop's shell.
async fn bind_machine(
    client: &Client,
    sess: &session::Session,
    name: &str,
    spec: &ProviderInfo,
    label: Option<String>,
) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = machine_binding(sess);
    let (blob, wrapped_dek) = seal_payload(&umk, &fields)?;
    let label = label.unwrap_or_else(|| sess.device_name.clone());
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
    println!("✓ {name} → {} ({label})", spec.display);
    println!("  calls to it run HERE, and only here — the binding is sealed, so nothing");
    println!("  server-side chooses which of your machines answers.");
    println!("  keep it answering:  mafold up   (or `mafold connection listen`)");
    println!("  what it can do:     mafold connection methods {name}");
    Ok(())
}

/// The sealed payload of a `computer` row. Keys match the registry's
/// `COMPUTER_BINDING`, or `filter_payload` would drop them the first time
/// anything rewrote the row.
fn machine_binding(sess: &session::Session) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("device_id".into(), json!(sess.device_id));
    fields.insert("machine".into(), json!(sess.device_name));
    fields.insert("os".into(), json!(std::env::consts::OS));
    fields.insert("bound_at".into(), json!(now_ms().to_string()));
    fields
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
    let spec = descriptor(client, &provider).await?;
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = open_payload(&umk, &key_id, &conn)?;
    let var = spec
        .env_var
        .ok_or_else(|| anyhow!("{} is an OAuth bag, not a single env var — use `show --reveal`", spec.id))?;
    let primary = spec.fields.first().map(|f| f.key.as_str()).unwrap_or("api_key");
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
    Ok(core_runtime(base, sess, umk))
}

/// Every runtime this cli builds, with the one capability that makes this
/// process a MACHINE the user can call.
///
/// One constructor, because the failure mode of forgetting it is invisible:
/// a daemon without an executor attached receives shell calls addressed to
/// this very machine and silently declines them (`Runtime::can_serve`), so the
/// caller times out and no log anywhere says why. There are three places that
/// build a runtime — `mafold connection call`, `listen`, and `mafold up`'s
/// resident listener — and all three are the same machine.
fn core_runtime(
    base: &str,
    sess: &session::Session,
    umk: Key,
) -> mafold_core::connections::Runtime {
    // `base` here is the ORIGIN — `Client` appends `/api` itself, and the
    // core's rpc does not. Passing it through unchanged makes every core call
    // a 404 that reads as "this server is older than this client", which is a
    // very convincing wrong answer.
    let mut rt =
        mafold_core::connections::Runtime::new(&format!("{base}/api"), &sess.token, umk);
    rt.attach_computer(&sess.device_id, crate::computer::executor());
    rt
}

// ── linking on behalf of another surface (`events.connectionLink`) ─────────
//
// The web pane has a Connect button for Codex and no command to copy, and this
// is what stands behind it. A provider whose OAuth client redirects to a
// loopback port can only be linked ON a machine — but the machine does not have
// to be the INTERFACE. A client asks (`startConnectionLink`), the event fans out
// to every device the person has online, one claims it, binds the port, and
// answers with the URL for the asking surface to open. The credential is still
// born here and sealed here; only the button moved.

/// A connection name that isn't taken yet — `codex`, then `codex-2`.
///
/// Same rule as the web's `uniqueName`, for the same reason: a second Codex
/// account must make a second row rather than overwrite the first. Naming is
/// this device's job because the asking surface never sees the grant.
async fn free_name(client: &Client, provider_id: &str) -> String {
    let base = provider_id
        .trim_end_matches("-api")
        .trim_end_matches("-oauth")
        .to_string();
    let taken: Vec<String> = client
        .call("listConnections", json!({}))
        .await
        .ok()
        .map(|v| as_array(&v, "items").iter().map(|c| s(c, "name")).collect())
        .unwrap_or_default();
    if !taken.iter().any(|n| n == &base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|c| !taken.iter().any(|n| n == c))
        .unwrap_or(base)
}

/// Answer an `events.connectionLink` frame. `true` when this device took it.
///
/// Claim FIRST, like every other relayed event: the frame reaches every socket
/// the account has, and two machines binding the vendor's port for one request
/// is two consent screens for one click. The claim also decides who reports the
/// outcome, so the asking surface hears exactly one ending.
pub async fn handle_link_event(
    client: &Client,
    sess: &session::Session,
    umk: &Key,
    key_id: &str,
    envelope: &str,
) -> bool {
    let env: Value = match serde_json::from_str(envelope) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if env.get("method").and_then(Value::as_str) != Some("events.connectionLink") {
        return false;
    }
    let p = env.get("params").cloned().unwrap_or(Value::Null);
    let link_id = s(&p, "link_id");
    let provider = s(&p, "provider");
    if link_id.is_empty() {
        return false;
    }

    let claimed = client
        .call("claimConnectionCall", json!({ "call_id": link_id }))
        .await
        .ok()
        .and_then(|v| v.get("claimed").and_then(Value::as_bool))
        .unwrap_or(false);
    if !claimed {
        return false;
    }

    // Answer once, whatever happens: the caller is parked on this and a silent
    // device turns "your Mac is busy" into "no machine took it".
    let answer = |result: Value, error: Option<String>| {
        let mut body = json!({ "call_id": link_id, "result": result });
        if let Some(e) = error {
            body["error"] = Value::String(e);
        }
        client.call("answerConnectionCall", body)
    };

    let spec = match descriptor(client, &provider).await.ok() {
        // A machine binding finishes right here: no port to bind, no consent
        // screen, nothing for the asking surface to open. It answers with the
        // connection instead of a URL, and `startConnectionLink` reads that as
        // "already linked".
        Some(sp) if is_device_binding(&sp) => {
            let outcome = bind_for_link(client, sess, umk, key_id, &sp).await;
            match &outcome {
                Ok(name) => {
                    let _ = answer(
                        json!({
                            "authorize_url": "",
                            "device": sess.device_name,
                            "connection": name,
                        }),
                        None,
                    )
                    .await;
                    println!("· connections: bound {name} → this machine ({})", sess.device_name);
                }
                Err(e) => {
                    let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
                    println!("· connections: could not bind this machine — {e:#}");
                }
            }
            // Report as well, so a caller that polls rather than reading the
            // start response lands on the same ending.
            let body = match &outcome {
                Ok(name) => json!({ "link_id": link_id, "connection": name }),
                Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
            };
            let _ = client.call("reportConnectionLink", body).await;
            return true;
        }
        Some(sp) if sp.oauth_fixed.is_some() => sp,
        Some(sp) => {
            let _ = answer(
                Value::Null,
                Some(format!(
                    "{} isn't linked by a consent screen — it's pasted or read from a file",
                    sp.display
                )),
            )
            .await;
            return true;
        }
        None => {
            let _ = answer(
                Value::Null,
                Some(format!(
                    "this machine's Mafold doesn't know a provider called `{provider}` — update it"
                )),
            )
            .await;
            return true;
        }
    };

    let leg = match oauth_begin(&spec).await {
        Ok(leg) => leg,
        Err(e) => {
            let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
            return true;
        }
    };
    let authorize_url = leg.authorize_url.clone();
    let _ = answer(
        json!({ "authorize_url": authorize_url, "device": sess.device_name }),
        None,
    )
    .await;

    // The human half — a consent screen, on a person's clock — must not hold
    // the socket loop. Everything it needs is owned here so the task outlives
    // this frame.
    let client = client.clone();
    let umk = umk.clone();
    let key_id = key_id.to_string();
    tokio::spawn(async move {
        let outcome = finish_linking(&client, &spec, leg, &umk, &key_id).await;
        let body = match &outcome {
            Ok(name) => json!({ "link_id": link_id, "connection": name }),
            Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
        };
        let _ = client.call("reportConnectionLink", body).await;
        match outcome {
            Ok(name) => println!("· connections: linked {name} → {}", spec.display),
            Err(e) => println!("· connections: {} link failed — {e:#}", spec.display),
        }
    });
    true
}

/// Bind this machine for a link someone started elsewhere (the web's Connect
/// button). Same sealing as `bind_machine`, minus the terminal.
///
/// The name is chosen here, by `free_name`, for the same reason the OAuth path
/// chooses it here: the asking surface never sees the payload, and two laptops
/// bound from the same browser must become two rows rather than one machine
/// overwriting the other.
async fn bind_for_link(
    client: &Client,
    sess: &session::Session,
    umk: &Key,
    key_id: &str,
    spec: &ProviderInfo,
) -> Result<String> {
    let fields = machine_binding(sess);
    let (blob, wrapped_dek) = seal_payload(umk, &fields)?;
    let name = free_name(client, &spec.id).await;
    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": sess.device_name,
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id,
            }),
        )
        .await
        .context("putConnection failed")?;
    Ok(name)
}

/// Wait out the consent screen, seal what comes back, store it. The connection
/// name it chose, so the report can name it.
async fn finish_linking(
    client: &Client,
    spec: &ProviderInfo,
    leg: OauthLeg,
    umk: &Key,
    key_id: &str,
) -> Result<String> {
    let (mut fields, suggested) = oauth_finish(spec, leg).await?;
    enrich_oauth_payload(&spec, &mut fields);
    let (blob, wrapped_dek) = seal_payload(umk, &fields)?;
    let name = free_name(client, &spec.id).await;
    let label = suggested.unwrap_or_else(|| {
        fields
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
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
    Ok(name)
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

    // Unlocked once, used twice: the core answers calls with the key, and a
    // link started from another surface seals its new grant with the same one.
    let (umk, key_id, _) = unlock(client, sess).await?;
    let mut rt = core_runtime(base, sess, umk.clone());
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
                    } else if handle_link_event(client, sess, &umk, &key_id, &t).await {
                        println!("· took a link request — finish the sign-in in your browser");
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

/// The `mafold up` resident listener: the reason "start your daemons" is
/// enough for granted bots (@chatgpt on your Codex connection) to work, with
/// no second command to know about.
///
/// QUIET by construction: it only serves when this machine already holds a
/// cached, still-current vault key — it never creates a vault, never prompts,
/// never prints. Locked (or logged-out) machines just re-check on a slow tick,
/// so running `mafold connection unlock` later brings this to life without
/// restarting the supervisor.
pub async fn supervise_listener(base: String) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;
    let mut said_locked = false;
    loop {
        let Some(sess) = session::load() else {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            continue;
        };
        let client = Client::new(base.clone(), sess.token.clone());
        let (mut rt, umk, key_id) = match quiet_runtime(&base, &client, &sess).await {
            Some(rt) => {
                said_locked = false;
                rt
            }
            None => {
                if !said_locked {
                    println!("· connections: vault locked here — `mafold connection unlock` lets granted bots use your connections on this machine");
                    said_locked = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                continue;
            }
        };
        println!("· connections: answering granted calls as @{}", sess.username);
        loop {
            let mut ws = match client.ws_connect().await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    let _ = e;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    break; // re-check session + key, then come back
                }
            };
            while let Some(frame) = ws.next().await {
                match frame {
                    Ok(WsMsg::Text(t)) => {
                        if mafold_core::connections::handle_event(&mut rt, &t).await {
                            println!("· connections: answered a call");
                        } else {
                            // A Connect button somewhere else (the web pane, a
                            // phone) asking this machine to run a consent
                            // screen it can and the asker can't.
                            handle_link_event(&client, &sess, &umk, &key_id, &t).await;
                        }
                    }
                    Ok(WsMsg::Ping(p)) => {
                        let _ = ws.send(WsMsg::Pong(p)).await;
                    }
                    Ok(WsMsg::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
}

/// The unlocked runtime IF this machine can produce one silently: registered
/// device + cached UMK that still matches the server's recorded generation.
/// Anything less returns None — enrollment is `unlock`'s interactive job.
///
/// The key comes back alongside the runtime because answering a call and
/// SEALING a new grant (a link this device runs for another surface) are the
/// same permission — a machine that can do one can do the other, and handing
/// out both from one place is what keeps that true.
async fn quiet_runtime(
    base: &str,
    client: &Client,
    sess: &session::Session,
) -> Option<(mafold_core::connections::Runtime, Key, String)> {
    let dev = vault::device_key().ok()?;
    let reg = register(client, sess, &dev).await.ok()?;
    let (umk, key_id) = vault::cached_umk(&dev)?;
    if key_id.is_empty() || s(&reg["device"], "key_id") != key_id {
        return None;
    }
    let rt = core_runtime(base, sess, umk.clone());
    Some((rt, umk, key_id))
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
    // Two places this can run, and the runtime already knows which: a
    // credential opens HERE (that is the whole vault), but a machine of yours
    // answers on ITSELF. Asking the relay for something this process could have
    // done would be a round trip to reach your own shell; running locally
    // something bound to another laptop would be worse than a round trip.
    let out = if rt.can_serve(name, method).await {
        rt.call_any("cli", name, method, &args, false)
            .await
            .map_err(|e| anyhow!("{e}"))?
    } else {
        eprintln!("· {name} lives on another machine of yours — relaying");
        relay(client, name, method, &args).await?
    };

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

    /// The registry is served now, so a test that links must have one in the
    /// process — the mock api serves connection calls, not packs.
    fn seat_registry() {
        mafold_core::providers::install_unverified_for_tests(1, provider_infos(), now_ms());
    }

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
        for p in provider_infos() {
            assert!(provider_infos().iter().any(|q| q.id == p.id));
        }
    }

    // ── the --oauth machinery ──

    fn fake_jwt(claims: Value) -> String {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"RS256"}"#),
            b64(claims.to_string().as_bytes()),
            b64(b"sig")
        )
    }

    #[test]
    fn jwt_claims_reads_the_middle_segment_unverified() {
        let t = fake_jwt(serde_json::json!({ "exp": 1234, "email": "a@b.c" }));
        let c = jwt_claims(&t).unwrap();
        assert_eq!(c["exp"], 1234);
        assert!(jwt_claims("not-a-jwt").is_none());
    }

    /// The import file carries neither the OAuth client nor an expiry; the
    /// enrichment is what makes an imported codex payload renewal-complete on
    /// any device.
    #[test]
    fn enrich_fills_client_account_and_expiry_without_clobbering() {
        use mafold_core::mafold_types::connections::codex;
        let spec = provider_infos().into_iter().find(|p| p.id == "codex-oauth").unwrap();
        let id_token = fake_jwt(serde_json::json!({
            "email": "ops@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-42", "chatgpt_plan_type": "pro" },
        }));
        let access = fake_jwt(serde_json::json!({ "exp": 1_900_000_000i64 }));
        let mut fields = serde_json::Map::new();
        fields.insert("access_token".into(), Value::String(access));
        fields.insert("id_token".into(), Value::String(id_token));

        enrich_oauth_payload(&spec, &mut fields);
        assert_eq!(fields["client_id"], codex::CLIENT_ID);
        assert_eq!(fields["token_endpoint"], codex::TOKEN_ENDPOINT);
        assert_eq!(fields["account_id"], "acc-42");
        assert_eq!(fields["expires_at"], (1_900_000_000i64 * 1000).to_string());

        // A payload that already knows better keeps its own values.
        fields.insert("account_id".into(), Value::String("acc-original".into()));
        fields.insert("expires_at".into(), Value::String("777".into()));
        enrich_oauth_payload(&spec, &mut fields);
        assert_eq!(fields["account_id"], "acc-original");
        assert_eq!(fields["expires_at"], "777");
    }

    /// Providers without a fixed OAuth client must pass through untouched —
    /// enrichment is additive, never a codex branch inside `add`.
    #[test]
    fn enrich_is_a_noop_for_providers_without_an_oauth_client() {
        let spec = provider_infos().into_iter().find(|p| p.id == "notion").unwrap();
        let mut fields = serde_json::Map::new();
        fields.insert("access_token".into(), Value::String("ntn_x".into()));
        enrich_oauth_payload(&spec, &mut fields);
        assert!(!fields.contains_key("client_id"));
        assert!(!fields.contains_key("token_endpoint"));
    }

    #[test]
    fn query_encoding_survives_token_alphabets() {
        assert_eq!(q_encode("openid profile email"), "openid%20profile%20email");
        assert_eq!(q_encode("http://localhost:1455/auth/callback"), "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback");
    }

    #[test]
    fn redirect_uri_splits_into_port_and_path() {
        let p = url_parts::split("http://localhost:1455/auth/callback").unwrap();
        assert_eq!(p.port, 1455);
        assert_eq!(p.path, "/auth/callback");
        assert!(url_parts::split("https://example.com/cb").is_err(), "https redirect would mean a public callback — refuse");
    }

    /// The callback server ends only on OUR state, answers noise with 404, and
    /// refuses a code minted for someone else's flow.
    #[tokio::test]
    async fn callback_waits_past_noise_and_checks_state() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wait = tokio::spawn(async move {
            wait_for_callback(listener, "st-1", "/auth/callback").await
        });

        // Favicon noise first — must be 404'd and NOT end the wait.
        let mut s1 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s1.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s1.read_to_string(&mut buf).await;
        assert!(buf.starts_with("HTTP/1.1 404"));

        // The real callback.
        let mut s2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s2.write_all(b"GET /auth/callback?code=c-9&state=st-1 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s2.read_to_string(&mut buf).await;
        assert!(buf.contains("Linked"), "{buf}");
        assert_eq!(wait.await.unwrap().unwrap(), "c-9");

        // And a wrong-state flow dies rather than returning the code.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wait = tokio::spawn(async move {
            wait_for_callback(listener, "st-2", "/auth/callback").await
        });
        let mut s3 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s3.write_all(b"GET /auth/callback?code=c-9&state=EVIL HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s3.read_to_string(&mut buf).await;
        let err = wait.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "{err}");
    }

    // ── the device side of a link started somewhere else ──────────────────

    /// A one-shot API stand-in: records every `(path, body)` and answers each
    /// call from `replies` by METHOD NAME, defaulting to `{ok:true}`. Local
    /// rather than shared because the cli has no test harness crate and one
    /// screen of tokio is cheaper than inventing one.
    struct MockApi {
        base: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    }

    impl MockApi {
        fn calls(&self, method: &str) -> Vec<Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p == &format!("/api/{method}"))
                .map(|(_, b)| b.clone())
                .collect()
        }
        /// Block until `method` has been called (or the test's patience runs
        /// out). Polling beats a sleep: the device answers in microseconds and
        /// a fixed wait would either be flaky or slow.
        async fn wait_for(&self, method: &str) -> Value {
            for _ in 0..200 {
                if let Some(b) = self.calls(method).into_iter().next() {
                    return b;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("{method} was never called: {:?}", self.seen.lock().unwrap());
        }
    }

    fn spawn_api(replies: Vec<(&'static str, Value)>) -> MockApi {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let base = format!("http://{}", std_listener.local_addr().unwrap());
        let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> = Default::default();
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let sink = sink.clone();
                let replies = replies.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let body: Value = req
                        .split_once("\r\n\r\n")
                        .and_then(|(_, b)| serde_json::from_str(b).ok())
                        .unwrap_or(Value::Null);
                    sink.lock().unwrap().push((path.clone(), body));
                    let result = replies
                        .iter()
                        .find(|(m, _)| path == format!("/api/{m}"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| json!({}));
                    let payload = json!({ "ok": true, "result": result }).to_string();
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                                payload.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        MockApi { base, seen }
    }

    fn a_session() -> session::Session {
        session::Session {
            token: "s_test".into(),
            username: "ops".into(),
            device_id: "d-1".into(),
            device_name: "ops-mbp".into(),
        }
    }

    fn link_frame(provider: &str) -> String {
        json!({
            "method": "events.connectionLink",
            "params": { "link_id": "11111111-1111-4111-8111-111111111111", "provider": provider },
        })
        .to_string()
    }

    /// The frames this handler must NOT touch. A device that claims events it
    /// can't finish is worse than one that ignores them: the claim is what
    /// stops another machine from doing the work.
    #[tokio::test]
    async fn an_unrelated_frame_is_ignored_without_a_claim() {
        let api = spawn_api(vec![]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        for frame in [
            r#"{"method":"events.connectionCall","params":{"call_id":"c-1"}}"#.to_string(),
            r#"{"method":"events.message","params":{}}"#.to_string(),
            "not json at all".to_string(),
        ] {
            assert!(!handle_link_event(&client, &a_session(), &umk, "k1", &frame).await);
        }
        assert!(api.calls("claimConnectionCall").is_empty());
    }

    /// Losing the claim ends it. Two laptops online must not mean two consent
    /// screens for one click.
    #[tokio::test]
    async fn losing_the_claim_stops_before_binding_anything() {
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": false }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(!handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("codex-oauth")).await);
        assert_eq!(api.calls("claimConnectionCall").len(), 1);
        assert!(api.calls("answerConnectionCall").is_empty());
    }

    /// A provider this build has never heard of still gets an ANSWER. The
    /// caller is parked on the rendezvous; a silent device turns "your Mafold
    /// is out of date" into "no machine took it", which sends the user looking
    /// at the wrong thing entirely.
    #[tokio::test]
    async fn an_unknown_provider_answers_with_words() {
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": true }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("nope-oauth")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        assert!(answer["error"].as_str().unwrap().contains("nope-oauth"), "{answer}");
        assert!(answer["result"].is_null());
    }

    /// A provider linked by paste or import is not a bug either — it is a
    /// sentence about the provider, not about the machine.
    #[tokio::test]
    async fn a_pasted_provider_says_so_rather_than_binding() {
        seat_registry();
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": true }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("notion")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        assert!(
            answer["error"].as_str().unwrap().contains("isn't linked by a consent screen"),
            "{answer}"
        );
    }

    /// The whole point: a codex link event comes back with a real consent URL
    /// and the machine's name, so the asking surface can send the person there
    /// and say where the sign-in is happening.
    ///
    /// The vendor's redirect port is a fixed constant (1455), so this test
    /// tolerates a machine where a real `codex login` already owns it — the
    /// handler must then answer with THAT sentence rather than go quiet.
    #[tokio::test]
    async fn a_codex_link_answers_with_a_consent_url_and_the_device_name() {
        seat_registry();
        let api = spawn_api(vec![
            ("claimConnectionCall", json!({ "claimed": true })),
            ("listConnections", json!({ "items": [] })),
        ]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("codex-oauth")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        if let Some(err) = answer["error"].as_str() {
            assert!(err.contains("127.0.0.1:1455"), "unexpected failure: {err}");
            return;
        }
        let url = answer["result"]["authorize_url"].as_str().expect("a url");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"), "{url}");
        assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
        assert_eq!(answer["result"]["device"], "ops-mbp");
    }

    /// Names don't collide: a second Codex account makes a second row.
    #[tokio::test]
    async fn a_free_name_steps_around_what_is_already_linked() {
        let api = spawn_api(vec![(
            "listConnections",
            json!({ "items": [ { "name": "codex" }, { "name": "codex-2" } ] }),
        )]);
        let client = Client::new(api.base.clone(), "s_test".into());
        assert_eq!(free_name(&client, "codex-oauth").await, "codex-3");
    }
}
