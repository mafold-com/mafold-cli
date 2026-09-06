//! Linking an MCP server the USER named — the `mcp` row's link flows.
//!
//! Every other provider tells this cli what to collect through the registry:
//! a file to import, a variable to read, a fixed OAuth client to drive. A
//! server the user typed tells us nothing in advance, so **this module asks
//! the server** ([`probe`]) and lets its answers pick the flow:
//!
//! ```text
//!   RFC 9728 → 8414 → 7591    it has an authorization server that registers
//!                             clients: consent screen, PKCE, exchange — here
//!   initialize → 200          needs no credential: the address alone is sealed
//!   initialize → 401          wants a token: ask for one (terminal), or say
//!                             so (a link that began on another surface)
//! ```
//!
//! Discovery comes FIRST, before any `initialize`, because the discovery
//! documents are the one thing a browser can also fetch across origins
//! (measured 2026-09-04: 8 of 8 served CORS, against 10 of 12 for `/mcp`
//! itself — Stripe and Sentry serve the documents and not the endpoint). This
//! cli and the web pane therefore classify a server the same way, which is
//! what lets a link that began in a browser finish on this machine without
//! the two disagreeing about what the server is.
//!
//! The redirect for a dynamically registered client is an EPHEMERAL loopback
//! port (RFC 8252 §7.3): bound first, registered second, because the
//! registration request must name it. Each link registers a fresh client; a
//! link is a once-per-server act, and the alternative is a store of client
//! ids keyed by server that outlives the port they were registered with.
//!
//! What is sealed, and why the address is in there: `.docs/custom-mcp-v1.md`
//! §2–3. Nothing in this file writes cleartext anywhere but the label.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::client::Client;
use crate::connection::{
    free_name_from, oauth_exchange, oauth_leg, seal_payload, unlock, OauthClient, OauthLeg,
};
use crate::session;
use crate::vault::Key;
use mafold_core::mafold_types::connections::{ProviderInfo, BEARER};
use mafold_core::mcp::{McpClient, McpError};

/// The authorization server an MCP server pointed at — the three endpoints a
/// dynamic-registration dance needs and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
}

/// What a server turned out to be, from talking to it.
#[derive(Debug)]
pub(crate) enum Probe {
    /// It will register us as a public client: the whole dance runs here.
    OAuth(AuthServer),
    /// It has an authorization server, but no door a client of ours can use.
    /// Carries the reason, in words a person can act on.
    OAuthClosed(String),
    /// `initialize` succeeded with no credential at all.
    Open,
    /// `initialize` was refused, and there is no discovery to follow.
    Token,
}

/// How long any single request to the server may take. Generous because the
/// first is a cold TLS handshake to a host nobody has warmed up; short enough
/// that a typo'd hostname does not hang a terminal.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("a default reqwest client builds")
}

// MARK: - URL shapes

/// `https://host[:port]` and the path (`/` when there is none).
///
/// Hand-split rather than a URL crate for the same reason `connection.rs`'s
/// `url_parts` is: two string operations do not justify a dependency, and the
/// inputs are already constrained to http(s) by the api's own check.
pub(crate) fn split_origin(url: &str) -> Result<(String, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("`{url}` isn't a URL — include the scheme (https://…)"))?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        bail!("`{url}`: only http(s) servers can be linked");
    }
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // Query and fragment belong to neither half.
    let host = host.split(['?', '#']).next().unwrap_or("");
    if host.is_empty() {
        bail!("`{url}` has no host");
    }
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let path = if path.is_empty() { "/" } else { path };
    Ok((format!("{}://{host}", scheme.to_ascii_lowercase()), path.to_string()))
}

/// A URL as typed, made linkable: trimmed, scheme required, shape checked.
pub(crate) fn normalize(url: &str) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        bail!("no server address given");
    }
    split_origin(url)?;
    Ok(url.to_string())
}

/// `mcp.stripe.com` — what the row is labelled with by default.
///
/// The host is the one part of the address a person recognises at a glance,
/// and it is the ONE cleartext thing this flow chooses to reveal (owner ruling
/// 2026-09-04: a readable list over a masked one). The port is kept when there
/// is one, because `localhost:3000` and `localhost:3001` are different servers.
pub(crate) fn host_of(url: &str) -> String {
    split_origin(url.trim())
        .map(|(origin, _)| {
            origin
                .split_once("://")
                .map(|(_, h)| h.to_string())
                .unwrap_or(origin)
        })
        // Not a URL at all: no host to name. Callers pass normalized
        // addresses, so this is the empty string a suggestion falls back from.
        .unwrap_or_default()
}

/// A connection name to suggest: `stripe` for `mcp.stripe.com`.
///
/// Leading labels that name the *service shape* rather than the vendor
/// (`mcp.`, `api.`, `www.`, `app.`) are skipped, the public suffix is not
/// guessed at (the first remaining label is the vendor for every host this
/// was measured against), and anything that does not slug cleanly falls back
/// to the row's own id — a suggestion is a suggestion.
pub(crate) fn suggested_name(url: &str) -> String {
    let host = host_of(url);
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    let mut pick = labels.first().copied().unwrap_or("");
    if labels.len() > 1 {
        let skip = ["mcp", "api", "www", "app", "server", "remote"];
        pick = labels
            .iter()
            .copied()
            .find(|l| !skip.contains(l))
            .unwrap_or(labels[0]);
    }
    let slug: String = pick
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() || slug.chars().all(|c| c.is_ascii_digit()) {
        "mcp".to_string()
    } else {
        slug
    }
}

// MARK: - Discovery

/// Where RFC 9728 says a server's protected-resource metadata may live, in
/// the order to try: the path-inserted form first (what a server that guards
/// several resources serves), then the bare one.
pub(crate) fn resource_metadata_candidates(endpoint: &str) -> Result<Vec<String>> {
    let (origin, path) = split_origin(endpoint)?;
    let mut out = Vec::new();
    let path = path.trim_end_matches('/');
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
    }
    out.push(format!("{origin}/.well-known/oauth-protected-resource"));
    Ok(out)
}

/// Where RFC 8414 says an issuer's metadata may live, in the order to try.
///
/// The PATH-INSERTED form comes first, and it is not a formality: Stripe's
/// issuer is `https://access.stripe.com/mcp`, whose metadata is served at
/// `/.well-known/oauth-authorization-server/mcp` and NOT at
/// `/mcp/.well-known/oauth-authorization-server` (404, measured 2026-09-04).
/// A client that only tries the suffix form cannot link Stripe at all.
pub(crate) fn issuer_metadata_candidates(issuer: &str) -> Result<Vec<String>> {
    let issuer = issuer.trim_end_matches('/');
    let (origin, path) = split_origin(issuer)?;
    let path = path.trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/oauth-authorization-server{path}"));
    }
    out.push(format!("{issuer}/.well-known/oauth-authorization-server"));
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/openid-configuration{path}"));
    }
    out.push(format!("{issuer}/.well-known/openid-configuration"));
    Ok(out)
}

enum Fetched {
    Found(Value),
    Missing,
}

/// One discovery GET. A network failure is an error — the host is the one the
/// person typed, and if it cannot be reached nothing after this can work. Any
/// non-2xx, or a body that is not a JSON object, is simply "not here".
async fn get_json(http: &reqwest::Client, url: &str) -> Result<Fetched> {
    let resp = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .with_context(|| format!("couldn't reach {}", host_of(url)))?;
    if !resp.status().is_success() {
        return Ok(Fetched::Missing);
    }
    Ok(match resp.json::<Value>().await {
        Ok(v) if v.is_object() => Fetched::Found(v),
        _ => Fetched::Missing,
    })
}

/// The authorization server's metadata for an MCP endpoint, if it has one.
///
/// `None` is a real answer ("no OAuth here"), not a failure: a server that
/// needs no credential, or one that wants a pasted token, has no such
/// documents and is classified by `initialize` instead.
pub(crate) async fn discover(endpoint: &str) -> Result<Option<Value>> {
    let http = http();
    let (origin, _) = split_origin(endpoint)?;
    // The MCP server names its authorization server. When it publishes
    // nothing, the origin itself is tried as the issuer — what Notion's shape
    // looked like before it published the document, and what the browser
    // does too.
    let mut issuer = origin;
    for c in resource_metadata_candidates(endpoint)? {
        if let Fetched::Found(v) = get_json(&http, &c).await? {
            if let Some(a) = v["authorization_servers"][0].as_str() {
                issuer = a.trim_end_matches('/').to_string();
            }
            break;
        }
    }
    for c in issuer_metadata_candidates(&issuer)? {
        if let Fetched::Found(v) = get_json(&http, &c).await? {
            if v["authorization_endpoint"].is_string() && v["token_endpoint"].is_string() {
                return Ok(Some(v));
            }
        }
    }
    Ok(None)
}

/// Whether an authorization server will take us as a public client.
///
/// Three things have to be true, and each has a sentence for when it isn't,
/// because "OAuth isn't supported" would send the person to look for a
/// setting on their side that does not exist:
///
///  * it registers clients (RFC 7591) — the alternative is a client id only
///    the vendor can hand out;
///  * it issues clients that authenticate with `none` — otherwise a secret
///    would have to ship inside this cli, which is not a secret;
///  * it accepts S256 PKCE — the one thing standing between a loopback
///    redirect and any other process on the machine.
pub(crate) fn classify(meta: &Value) -> std::result::Result<AuthServer, String> {
    let text = |k: &str| {
        meta[k]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (Some(authorization_endpoint), Some(token_endpoint)) =
        (text("authorization_endpoint"), text("token_endpoint"))
    else {
        return Err("its authorization server metadata is incomplete".into());
    };
    let Some(registration_endpoint) = text("registration_endpoint") else {
        return Err("its authorization server doesn't register clients (no \
                    registration_endpoint), so it needs a client id only the vendor can issue"
            .into());
    };
    let lists = |k: &str| -> Option<Vec<String>> {
        meta[k].as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };
    if let Some(methods) = lists("token_endpoint_auth_methods_supported") {
        if !methods.iter().any(|m| m == "none") {
            return Err("its authorization server only issues confidential clients (no `none` \
                        auth method) — a client secret would have to ship with Mafold, which \
                        is not a secret"
                .into());
        }
    }
    if let Some(methods) = lists("code_challenge_methods_supported") {
        if !methods.iter().any(|m| m == "S256") {
            return Err("its authorization server doesn't accept S256 PKCE".into());
        }
    }
    Ok(AuthServer {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
    })
}

/// Ask the server what it needs.
pub(crate) async fn probe(endpoint: &str) -> Result<Probe> {
    if let Some(meta) = discover(endpoint).await? {
        return Ok(match classify(&meta) {
            Ok(auth) => Probe::OAuth(auth),
            Err(why) => Probe::OAuthClosed(why),
        });
    }
    // No authorization server. The handshake itself says whether a credential
    // is wanted — sent with NONE, which the core turns into "no header" rather
    // than an empty bearer a server would refuse as malformed.
    let mut client = McpClient::new(endpoint, &BEARER.into(), "");
    match client.initialize().await {
        Ok(_) => Ok(Probe::Open),
        Err(McpError::Unauthorized(_)) => Ok(Probe::Token),
        Err(McpError::Transport(m)) => {
            bail!("{} isn't answering as an MCP server: {m}", host_of(endpoint))
        }
        Err(McpError::Protocol(m)) => {
            bail!("{} answered, but refused the MCP handshake: {m}", host_of(endpoint))
        }
    }
}

// MARK: - The dance

/// Register this machine as a public client for one link. Returns the id.
async fn register(auth: &AuthServer, redirect_uri: &str) -> Result<String> {
    let resp = http()
        .post(&auth.registration_endpoint)
        .json(&json!({
            "client_name": "Mafold",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "native",
        }))
        .send()
        .await
        .context("couldn't reach the registration endpoint")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "the authorization server refused to register a client (HTTP {status}): {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let v: Value = serde_json::from_str(&body).context("registration answered non-JSON")?;
    v["client_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("registration answered without a client_id"))
}

/// Bind a loopback port, register a client that redirects to it, build the
/// consent URL. The listener rides inside the leg so the exchange can only
/// ever wait on the port the registration named.
pub(crate) async fn begin_oauth(endpoint: &str, auth: &AuthServer) -> Result<(OauthClient, OauthLeg)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("couldn't open a loopback port for the sign-in to come back to")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let client_id = register(auth, &redirect_uri).await?;
    let oc = OauthClient {
        client_id,
        authorize_url: auth.authorization_endpoint.clone(),
        token_endpoint: auth.token_endpoint.clone(),
        redirect_uri,
        // The server's default scope, same as the browser asks for. Naming
        // every scope it advertises would over-ask; naming none lets the
        // server pick what an MCP client gets.
        scopes: String::new(),
        extra_params: Vec::new(),
        resource: Some(endpoint.to_string()),
    };
    let leg = oauth_leg(&oc, listener)?;
    Ok((oc, leg))
}

/// Seal `fields` plus the address, and store the row.
///
/// Filtered against the row's `payload_keys` before sealing — the same rule
/// the core applies on renewal and the web applies at link time — so what is
/// stored here is exactly what every other writer would store.
async fn store(
    client: &Client,
    umk: &Key,
    key_id: &str,
    spec: &ProviderInfo,
    name: &str,
    endpoint: &str,
    mut fields: Map<String, Value>,
    label: &str,
) -> Result<()> {
    fields.insert("endpoint".into(), Value::String(endpoint.to_string()));
    let kept = mafold_core::connections::filter_payload(spec, &fields);
    let (blob, wrapped_dek) = seal_payload(umk, &kept)?;
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
    Ok(())
}

/// `mafold connection add <name> --provider mcp --url <server>`, end to end.
pub(crate) async fn add_server(
    client: &Client,
    sess: &session::Session,
    name: &str,
    spec: &ProviderInfo,
    endpoint: &str,
    auth_header: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let endpoint = normalize(endpoint)?;
    let host = host_of(&endpoint);
    println!("Asking {host} what it needs…");
    let mut fields = Map::new();
    match probe(&endpoint).await? {
        Probe::Open => println!("  no sign-in needed — it answers without a credential."),
        Probe::Token => {
            println!("  it wants a token.");
            let token = crate::prompt_password("Access token: ");
            let token = token.trim();
            if token.is_empty() {
                bail!("no token entered — nothing was linked");
            }
            fields.insert("access_token".into(), Value::String(token.to_string()));
            if let Some(h) = auth_header.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
                fields.insert("auth_header".into(), Value::String(h.to_string()));
            }
        }
        Probe::OAuthClosed(why) => bail!("{host} can't be linked yet: {why}"),
        Probe::OAuth(auth) => {
            let (oc, leg) = begin_oauth(&endpoint, &auth).await?;
            println!("Opening {host}'s sign-in…");
            if !crate::platform::open_browser(&leg.authorize_url) {
                println!(
                    "  couldn't open a browser — visit this URL yourself:\n\n  {}\n",
                    leg.authorize_url
                );
            }
            println!("  waiting for the sign-in to come back to 127.0.0.1:{}…", leg.port);
            let (bag, _) = oauth_exchange(&oc, leg).await?;
            fields.extend(bag);
            if auth_header.is_some() {
                println!("  (--auth-header ignored: the server issued its own credential)");
            }
        }
    }

    let (umk, key_id, _) = unlock(client, sess).await?;
    let label = label.unwrap_or_else(|| host.clone());
    store(client, &umk, &key_id, spec, name, &endpoint, fields, &label).await?;
    println!("✓ linked {name} → {host} ({label})");
    println!("  the server stored ciphertext it cannot open; only your enrolled devices can.");
    println!("  its methods:  mafold connection methods {name}");
    Ok(())
}

/// The device half of a link that began somewhere else (`events.connectionLink`
/// carrying an `endpoint`). Answers the parked `startConnectionLink` with one
/// of three endings, and reports the final outcome for the ones that have one.
///
/// Never returns an error: the asking surface is waiting on an ANSWER, and a
/// device that fails silently turns "your Mac hit a typo" into "no machine
/// took it".
///
/// `name` / `label` are what the person typed on the asking surface, when
/// they typed anything; the row is named from the server's host otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_link(
    client: &Client,
    sess: &session::Session,
    umk: &Key,
    key_id: &str,
    spec: &ProviderInfo,
    endpoint: &str,
    link_id: &str,
    name: Option<String>,
    label: Option<String>,
) {
    let answer = |result: Value, error: Option<String>| {
        let mut body = json!({ "call_id": link_id, "result": result });
        if let Some(e) = error {
            body["error"] = Value::String(e);
        }
        client.call("answerConnectionCall", body)
    };
    let endpoint = match normalize(endpoint) {
        Ok(e) => e,
        Err(e) => {
            let _ = answer(Value::Null, Some(format!("the link named no usable server: {e:#}"))).await;
            return;
        }
    };
    let host = host_of(&endpoint);
    // The typed name still gets the free-name treatment: a link from a form
    // that warned about a clash is one thing, silently replacing a row that
    // exists is another.
    let wanted = name.unwrap_or_else(|| suggested_name(&endpoint));
    let label = label.unwrap_or_else(|| host.clone());

    match probe(&endpoint).await {
        Err(e) => {
            let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
            println!("· connections: couldn't probe {host} — {e:#}");
        }
        Ok(Probe::OAuthClosed(why)) => {
            let _ = answer(Value::Null, Some(format!("{host} can't be linked yet: {why}"))).await;
            println!("· connections: {host} can't be linked yet — {why}");
        }
        // Nothing to open and nothing stored: the asking surface collects the
        // token and seals it where it is. No report — `startConnectionLink`
        // returns this ending directly, so there is nothing left to poll.
        Ok(Probe::Token) => {
            let _ = answer(json!({ "needs_token": true, "device": sess.device_name }), None).await;
            println!("· connections: {host} wants a token — the asking surface collects it");
        }
        // Done on the spot, the way a machine binding is: no consent screen,
        // so the answer carries the connection rather than a URL.
        Ok(Probe::Open) => {
            let name = free_name_from(client, &wanted).await;
            let outcome = store(client, umk, key_id, spec, &name, &endpoint, Map::new(), &label).await;
            match &outcome {
                Ok(()) => {
                    let _ = answer(
                        json!({ "authorize_url": "", "device": sess.device_name, "connection": name }),
                        None,
                    )
                    .await;
                    println!("· connections: linked {name} → {host} (no credential needed)");
                }
                Err(e) => {
                    let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
                    println!("· connections: couldn't store {host} — {e:#}");
                }
            }
            let body = match &outcome {
                Ok(()) => json!({ "link_id": link_id, "connection": name }),
                Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
            };
            let _ = client.call("reportConnectionLink", body).await;
        }
        Ok(Probe::OAuth(auth)) => {
            let (oc, leg) = match begin_oauth(&endpoint, &auth).await {
                Ok(x) => x,
                Err(e) => {
                    let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
                    println!("· connections: couldn't start {host}'s sign-in — {e:#}");
                    return;
                }
            };
            let _ = answer(
                json!({ "authorize_url": leg.authorize_url, "device": sess.device_name }),
                None,
            )
            .await;

            // The human half — a consent screen, on a person's clock — must
            // not hold the socket loop. Everything it needs is owned here so
            // the task outlives this frame.
            let client = client.clone();
            let umk = umk.clone();
            let key_id = key_id.to_string();
            let spec = spec.clone();
            let link_id = link_id.to_string();
            tokio::spawn(async move {
                let outcome: Result<String> = async {
                    let (bag, _) = oauth_exchange(&oc, leg).await?;
                    let name = free_name_from(&client, &wanted).await;
                    store(&client, &umk, &key_id, &spec, &name, &endpoint, bag, &label).await?;
                    Ok(name)
                }
                .await;
                let body = match &outcome {
                    Ok(name) => json!({ "link_id": link_id, "connection": name }),
                    Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
                };
                let _ = client.call("reportConnectionLink", body).await;
                match outcome {
                    Ok(name) => println!("· connections: linked {name} → {host}"),
                    Err(e) => println!("· connections: {host} link failed — {e:#}"),
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_paths_split_the_way_discovery_needs() {
        assert_eq!(
            split_origin("https://mcp.stripe.com/").unwrap(),
            ("https://mcp.stripe.com".into(), "/".into())
        );
        assert_eq!(
            split_origin("https://mcp.notion.com/mcp?x=1#f").unwrap(),
            ("https://mcp.notion.com".into(), "/mcp".into())
        );
        assert_eq!(
            split_origin("HTTP://localhost:3000").unwrap(),
            ("http://localhost:3000".into(), "/".into())
        );
        for bad in ["mcp.stripe.com", "ftp://x/y", "https://", ""] {
            assert!(split_origin(bad).is_err(), "{bad:?}");
        }
    }

    /// The label is the host, port included — `localhost:3000` and
    /// `localhost:3001` are two servers.
    #[test]
    fn the_label_is_the_host() {
        assert_eq!(host_of("https://mcp.stripe.com/"), "mcp.stripe.com");
        assert_eq!(host_of("http://localhost:3000/mcp"), "localhost:3000");
        assert_eq!(host_of(" https://api.githubcopilot.com/mcp/ "), "api.githubcopilot.com");
    }

    #[test]
    fn the_suggested_name_is_the_vendor_not_the_service_shape() {
        assert_eq!(suggested_name("https://mcp.stripe.com/"), "stripe");
        assert_eq!(suggested_name("https://mcp.sentry.dev/mcp"), "sentry");
        assert_eq!(suggested_name("https://api.githubcopilot.com/mcp/"), "githubcopilot");
        assert_eq!(suggested_name("https://huggingface.co/mcp"), "huggingface");
        assert_eq!(suggested_name("http://localhost:3000/mcp"), "localhost");
        // Nothing worth suggesting falls back to the row's own id.
        assert_eq!(suggested_name("http://192.168.1.5:8080/"), "mcp");
        assert_eq!(suggested_name("not a url"), "mcp");
    }

    /// Path-inserted forms FIRST. Stripe's issuer metadata lives only there.
    #[test]
    fn discovery_tries_the_path_inserted_form_before_the_suffix_form() {
        assert_eq!(
            resource_metadata_candidates("https://mcp.notion.com/mcp").unwrap(),
            vec![
                "https://mcp.notion.com/.well-known/oauth-protected-resource/mcp",
                "https://mcp.notion.com/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            resource_metadata_candidates("https://mcp.stripe.com/").unwrap(),
            vec!["https://mcp.stripe.com/.well-known/oauth-protected-resource"]
        );
        assert_eq!(
            issuer_metadata_candidates("https://access.stripe.com/mcp").unwrap(),
            vec![
                "https://access.stripe.com/.well-known/oauth-authorization-server/mcp",
                "https://access.stripe.com/mcp/.well-known/oauth-authorization-server",
                "https://access.stripe.com/.well-known/openid-configuration/mcp",
                "https://access.stripe.com/mcp/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            issuer_metadata_candidates("https://mcp.sentry.dev/").unwrap(),
            vec![
                "https://mcp.sentry.dev/.well-known/oauth-authorization-server",
                "https://mcp.sentry.dev/.well-known/openid-configuration",
            ]
        );
    }

    /// Stripe's metadata, verbatim from 2026-09-04: the shape this must accept.
    #[test]
    fn a_public_registering_server_is_linkable() {
        let meta = json!({
            "issuer": "https://access.stripe.com/mcp",
            "authorization_endpoint": "https://access.stripe.com/mcp/oauth2/authorize",
            "token_endpoint": "https://access.stripe.com/mcp/oauth2/token",
            "registration_endpoint": "https://access.stripe.com/mcp/oauth2/register",
            "token_endpoint_auth_methods_supported": ["none"],
            "code_challenge_methods_supported": ["S256"],
        });
        assert_eq!(
            classify(&meta).unwrap(),
            AuthServer {
                authorization_endpoint: "https://access.stripe.com/mcp/oauth2/authorize".into(),
                token_endpoint: "https://access.stripe.com/mcp/oauth2/token".into(),
                registration_endpoint: "https://access.stripe.com/mcp/oauth2/register".into(),
            }
        );
        // Sentry lists confidential methods AND `none`: still linkable.
        let sentry = json!({
            "authorization_endpoint": "https://mcp.sentry.dev/oauth/authorize",
            "token_endpoint": "https://mcp.sentry.dev/oauth/token",
            "registration_endpoint": "https://mcp.sentry.dev/oauth/register",
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
            "code_challenge_methods_supported": ["plain", "S256"],
        });
        assert!(classify(&sentry).is_ok());
        // Absent lists mean "no restriction stated" — the RFC's default.
        let terse = json!({
            "authorization_endpoint": "https://a/x",
            "token_endpoint": "https://a/t",
            "registration_endpoint": "https://a/r",
        });
        assert!(classify(&terse).is_ok());
    }

    /// Each closed door has its own sentence, naming the door.
    #[test]
    fn a_closed_server_says_which_door_is_shut() {
        let base = json!({
            "authorization_endpoint": "https://a/x",
            "token_endpoint": "https://a/t",
        });
        let err = classify(&base).unwrap_err();
        assert!(err.contains("registration_endpoint"), "{err}");

        let mut confidential = base.clone();
        confidential["registration_endpoint"] = json!("https://a/r");
        confidential["token_endpoint_auth_methods_supported"] = json!(["client_secret_post"]);
        let err = classify(&confidential).unwrap_err();
        assert!(err.contains("confidential"), "{err}");

        let mut plain_only = base.clone();
        plain_only["registration_endpoint"] = json!("https://a/r");
        plain_only["code_challenge_methods_supported"] = json!(["plain"]);
        let err = classify(&plain_only).unwrap_err();
        assert!(err.contains("S256"), "{err}");

        let incomplete = json!({ "registration_endpoint": "https://a/r" });
        assert!(classify(&incomplete).unwrap_err().contains("incomplete"));
    }

    /// A tiny MCP server with no discovery documents: `initialize` decides.
    async fn fake_server(initialize_status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = req.lines().next().unwrap_or("").to_string();
                    let (status, body) = if line.starts_with("GET ") {
                        (404u16, String::new())
                    } else if initialize_status == 200 {
                        (
                            200,
                            json!({ "jsonrpc": "2.0", "id": 1, "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": { "name": "fake", "version": "0" }
                            }})
                            .to_string(),
                        )
                    } else {
                        (initialize_status, "nope".to_string())
                    };
                    let reason = match status { 200 => "OK", 401 => "Unauthorized", _ => "Not Found" };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/mcp")
    }

    #[tokio::test]
    async fn a_server_with_no_discovery_is_classified_by_its_handshake() {
        let open = fake_server(200).await;
        assert!(matches!(probe(&open).await.unwrap(), Probe::Open));

        let guarded = fake_server(401).await;
        assert!(matches!(probe(&guarded).await.unwrap(), Probe::Token));

        // Not an MCP server at all: the handshake's own words come back.
        let missing = fake_server(404).await;
        let err = probe(&missing).await.unwrap_err().to_string();
        assert!(err.contains("isn't answering as an MCP server"), "{err}");
    }

    // ── live vendors ──
    //
    // Run by hand (`cargo test -- --ignored network_`) when the shapes pinned
    // above are suspected of having drifted. Each is a vendor the owner named
    // or one measured beside it on 2026-09-04; none needs an account.

    #[tokio::test]
    #[ignore = "talks to a live vendor"]
    async fn network_stripe_is_oauth_with_dynamic_registration() {
        match probe("https://mcp.stripe.com/").await.unwrap() {
            Probe::OAuth(a) => {
                assert_eq!(a.authorization_endpoint, "https://access.stripe.com/mcp/oauth2/authorize");
                assert_eq!(a.token_endpoint, "https://access.stripe.com/mcp/oauth2/token");
                assert_eq!(a.registration_endpoint, "https://access.stripe.com/mcp/oauth2/register");
            }
            other => panic!("Stripe should be linkable by OAuth, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "talks to a live vendor"]
    async fn network_sentry_is_oauth_with_dynamic_registration() {
        match probe("https://mcp.sentry.dev/mcp").await.unwrap() {
            Probe::OAuth(a) => {
                assert_eq!(a.registration_endpoint, "https://mcp.sentry.dev/oauth/register");
            }
            other => panic!("Sentry should be linkable by OAuth, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "talks to a live vendor"]
    async fn network_deepwiki_needs_no_credential() {
        assert!(matches!(probe("https://mcp.deepwiki.com/mcp").await.unwrap(), Probe::Open));
    }

    /// The whole first leg against Stripe: bind a loopback port, register a
    /// public client for it, build the consent URL. Stops short of the human
    /// half — the URL is asserted, never opened.
    #[tokio::test]
    #[ignore = "registers a throwaway public client at a live vendor"]
    async fn network_stripe_registers_a_public_client_for_a_loopback_redirect() {
        let Probe::OAuth(auth) = probe("https://mcp.stripe.com/").await.unwrap() else {
            panic!("Stripe should be linkable by OAuth");
        };
        let (oc, leg) = begin_oauth("https://mcp.stripe.com/", &auth).await.unwrap();
        assert!(!oc.client_id.is_empty());
        assert!(oc.redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(leg.authorize_url.starts_with(&auth.authorization_endpoint), "{}", leg.authorize_url);
        assert!(leg.authorize_url.contains(&format!("client_id={}", oc.client_id)), "{}", leg.authorize_url);
        assert!(leg.authorize_url.contains("resource=https%3A%2F%2Fmcp.stripe.com%2F"), "{}", leg.authorize_url);
        assert!(leg.authorize_url.contains("code_challenge_method=S256"));
        assert!(!leg.authorize_url.contains("scope="), "no scope means the server's default");
    }

    #[tokio::test]
    async fn a_host_that_is_not_there_is_an_error_not_a_classification() {
        // A port nothing listens on, so the failure is immediate.
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let err = probe(&format!("http://127.0.0.1:{port}/mcp")).await.unwrap_err().to_string();
        assert!(err.contains("couldn't reach"), "{err}");
    }
}
