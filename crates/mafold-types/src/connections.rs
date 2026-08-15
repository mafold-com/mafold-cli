//! Connections — the user's own credentials at third-party platforms, synced
//! through Mafold as **ciphertext the server cannot open**.
//!
//! This is a different layer from `connectors/` (the `@github` / `@notion` bot
//! accounts). A connector is an account that acts *server-side* on your behalf,
//! so the server necessarily holds a key it can use. A connection is the
//! opposite arrangement: the plaintext is only ever assembled on **your own
//! machines**, because the things that read it — a Claude Code harness, a Codex
//! harness, a local MCP server — run there. The server's whole job is to be a
//! synchronizing box of opaque bytes, so that linking an account once makes it
//! available to every daemon you run without ever making it available to us.
//!
//! Consequently **nothing in this file, and nothing in `mafold-api`, performs
//! encryption**. Sealing and opening live in `mafold-cli`'s vault module, on the
//! device that owns a key. If you ever find yourself needing a `Sealer` here to
//! make a feature work, the feature is asking the server to read a connection
//! and the answer is no.
//!
//! What the server *does* see is stated plainly in [`ConnectionMeta`]: a name, a
//! provider id, and a short human label. That leak is deliberate — it is what
//! makes `mafold connection list` readable — and it is the entire budget.

use serde::{Deserialize, Serialize};

// MARK: - Provider registry

/// How a provider is authenticated. Drives what `mafold connection add` asks
/// for, never how the bytes are stored — every provider's secret ends up in the
/// same sealed blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// A long-lived key the user pastes (or we lift from the environment).
    ApiKey,
    /// A token bag produced by an OAuth login the harness already performed;
    /// we import it rather than re-implementing someone else's login.
    OAuth,
}

/// Where a provider wants its credential on the wire.
///
/// This is **data, not a branch**, because the two providers that speak MCP
/// disagree and neither is wrong. Notion takes `Authorization: Bearer …`.
/// Figma's MCP server refuses that header outright — probed 2026-08-11, it
/// answers `figd_ tokens must be passed via X-Figma-Token header, not
/// Authorization`. Encoding that as three strings keeps both on one
/// request-building path in the core; encoding it as an `if provider == "figma"`
/// would put a provider name inside the transport, which is exactly the shape
/// §9 of the constitution exists to forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthStyle {
    /// Header the credential rides in.
    pub header: &'static str,
    /// What precedes the token in that header. Empty for raw-token headers.
    pub prefix: &'static str,
    /// Which key of the sealed payload holds the credential. Named rather than
    /// assumed, so a provider whose token is not `access_token` cannot silently
    /// send an empty string.
    pub field: &'static str,
}

/// RFC 6750 — the default, and what every OAuth provider expects.
pub const BEARER: AuthStyle = AuthStyle {
    header: "Authorization",
    prefix: "Bearer ",
    field: "access_token",
};

/// Figma's own header, carrying a personal access token verbatim.
pub const FIGMA_TOKEN_HEADER: AuthStyle = AuthStyle {
    header: "X-Figma-Token",
    prefix: "",
    field: "access_token",
};

/// A bearer token in the standard header, read from an API key rather than an
/// OAuth grant. Same wire shape as [`BEARER`], different field — which is
/// precisely why `field` is spelled out instead of assumed.
pub const BEARER_API_KEY: AuthStyle = AuthStyle {
    header: "Authorization",
    prefix: "Bearer ",
    field: "api_key",
};

/// Anthropic's platform takes its key in `x-api-key`, not `Authorization`.
pub const ANTHROPIC_API_KEY: AuthStyle = AuthStyle {
    header: "x-api-key",
    prefix: "",
    field: "api_key",
};

/// One field inside a connection's sealed payload.
#[derive(Debug, Clone, Copy)]
pub struct SecretField {
    pub key: &'static str,
    pub label: &'static str,
    /// A connection missing a required field is refused at `add` time rather
    /// than failing later inside a harness, where the error surfaces as a
    /// third-party 401 with no hint that Mafold had a half-filled row.
    pub required: bool,
    /// Written by the link flow, never typed by a human.
    ///
    /// These still belong in the sealed payload — `client_id` and
    /// `token_endpoint` are what let a *different* device refresh a token it
    /// did not obtain — but a form that rendered an input for them would be
    /// asking the user to invent an OAuth client. So the two lists diverge:
    /// [`ProviderInfo::fields`] is what to draw, [`ProviderInfo::payload_keys`]
    /// is what to keep.
    pub issued: bool,
}

/// A provider is **data**, not a branch. Adding one is a row here plus, if it
/// can be imported, a path — no changes to the store, the routes, or the CLI's
/// command surface. (Same discipline as `ConnectorManifest`; see
/// `.docs/connectors-v1.md` §6.)
#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub display: &'static str,
    /// One line on what linking this gets you — the row's second line in a UI.
    pub blurb: &'static str,
    /// Brand mark slug, served by the api as `/assets/bot/<badge>.png`. It names
    /// art that ALREADY exists for bots and connectors, so a Notion connection
    /// and the @notion connector wear one face and re-cutting a logo is one file.
    /// Empty = no mark; a client draws its own generic placeholder.
    pub badge: &'static str,
    pub kind: ProviderKind,
    pub fields: &'static [SecretField],
    /// A local file this credential can be lifted from, relative to `$HOME`.
    /// This is why `add --import` exists: for OAuth providers the user has
    /// *already* logged in through the vendor's own CLI, and asking them to
    /// paste a refresh token by hand would be both worse UX and worse security
    /// than reading the file the vendor just wrote.
    pub import_path: Option<&'static str>,
    /// Environment variable an API key conventionally arrives in, for
    /// `add --from-env`.
    pub env_var: Option<&'static str>,
    /// How this provider's credential is presented on a request. Only load
    /// bearing for providers we actually CALL (those with an `mcp_url`); the
    /// rest carry [`BEARER`] as an inert default rather than an `Option`
    /// nobody would remember to fill in when they gain a callable surface.
    pub auth: AuthStyle,
    /// A browser can link this with **no operator setup at all** — the
    /// authorization server accepts RFC 7591 dynamic registration *and* issues
    /// public clients, so the code→token exchange stays in the browser.
    ///
    /// Not "has OAuth". Figma has a perfectly good authorization server and is
    /// still `false`: its registration endpoint answers 403 to everyone, and
    /// its token endpoint advertises only `client_secret_basic` /
    /// `client_secret_post` — so linking it from a browser would need a secret
    /// we'd have to ship, which is not a secret. Probed 2026-08-11; re-probe
    /// before flipping this, don't assume the metadata means what it says.
    pub oauth_capable: bool,
    /// Where a human goes to mint the credential this provider wants.
    ///
    /// Data because it is the difference between a usable form and a dead end:
    /// "paste an access token" with no destination is a request the user cannot
    /// act on. Only meaningful for providers that are pasted rather than
    /// consented to — an OAuth flow lands the user in the right place by
    /// construction.
    pub help_url: Option<&'static str>,
    /// The provider's MCP server, when it has one.
    ///
    /// This single field replaces per-provider OAuth configuration entirely.
    /// An MCP server publishes its authorization server via
    /// `/.well-known/oauth-protected-resource`, and that server supports
    /// **dynamic client registration** — so a client registers itself, uses
    /// PKCE, and needs no client id, no client secret, and no operator setup.
    ///
    /// It is also the only OAuth shape that keeps the vault honest: a public
    /// client has no secret to protect, so the code→token exchange runs IN THE
    /// BROWSER and the server never sees a token. Confidential-client OAuth
    /// forces the exchange server-side, which is a real (if brief) hole in
    /// "the server cannot read your credentials". Prefer this wherever it
    /// exists — owner ruling, 2026-08-10.
    pub mcp_url: Option<&'static str>,
    /// A NATIVE driver in the core, for providers whose callable surface is
    /// not MCP. Data, not a branch: the row names its driver ("codex-responses")
    /// and the core keeps one dispatch table — an `if provider == "codex-oauth"`
    /// inside the transport is exactly the shape §9 forbids. `None` for the
    /// majority, whose callable surface is MCP or nothing.
    pub native_api: Option<&'static str>,
    /// A FIXED public OAuth client the cli can drive end-to-end (`add --oauth`),
    /// for vendors whose client id is a published constant of their own CLI
    /// rather than dynamically registered. The registered redirect is a
    /// localhost URI, so the dance can only land on a machine the user controls
    /// — which is exactly where the vault wants a credential born. `None` for
    /// providers linked by paste, env, import, or MCP dynamic registration.
    pub oauth_client: Option<OAuthClientSpec>,
}

/// See [`ProviderSpec::oauth_client`].
#[derive(Debug, Clone, Copy)]
pub struct OAuthClientSpec {
    pub client_id: &'static str,
    pub authorize_url: &'static str,
    /// Where the code→token exchange is POSTed. Normally the vendor's own
    /// endpoint; for a [`broker`](Self::broker)ed provider it is mafold-api,
    /// which is the only party holding the secret the vendor demands.
    pub token_endpoint: &'static str,
    /// The redirect registered at the vendor — a localhost URI the linking
    /// machine must be able to listen on.
    pub redirect_uri: &'static str,
    pub scopes: &'static str,
    /// Vendor-specific extra authorize-URL params, sent verbatim.
    pub extra_params: &'static [(&'static str, &'static str)],
    /// Set when the vendor REFUSES public clients, so the exchange cannot
    /// finish on the device. See [`BrokerSpec`]. `None` for every vendor whose
    /// client id is public — the honest case, and the one to prefer.
    pub broker: Option<BrokerSpec>,
}

/// Where mafold-api forwards a brokered exchange, and which secret it adds.
///
/// This exists because some authorization servers advertise no
/// `token_endpoint_auth_method` of `none`: every exchange and every renewal
/// must present a client secret. A secret shipped to five clients is not a
/// secret, so the only remaining place for it is the server — which means the
/// server briefly sees a token it can otherwise never read. That is the hole
/// `mcp_url`'s doc calls "real (if brief)", and taking it is a LAST RESORT:
/// allowed only once the MCP door is measured shut, never as a shortcut past
/// dynamic registration.
///
/// It is data rather than an `if provider == …` in the route because the next
/// vendor to refuse public clients must cost one row here and no new code.
#[derive(Debug, Clone, Copy)]
pub struct BrokerSpec {
    /// The vendor's real `authorization_code` endpoint.
    pub upstream_token: &'static str,
    /// The vendor's real `refresh_token` endpoint. Named separately because
    /// vendors disagree: Figma splits them (`/v1/oauth/token` vs
    /// `/v1/oauth/refresh`), most reuse one URL — in which case both fields
    /// carry the same string rather than an `Option` the broker must branch on.
    pub upstream_refresh: &'static str,
    /// Environment variable on the api host holding the client secret. Named,
    /// not derived from the provider id, so rotating or sharing a secret is a
    /// config change and never a rename.
    pub secret_env: &'static str,
}

// MARK: - Codex OAuth constants
//
// These are the Codex CLI's own public-client parameters, verbatim. They are
// CONSTANTS of that client, not configuration: the redirect URI is registered
// at OpenAI as `localhost:1455`, which is why the browser leg of this flow can
// only ever land on a machine the user controls — precisely the place the
// vault wants a credential born. The link flows (cli `--oauth` / `--import`)
// copy `client_id` + `token_endpoint` into the sealed payload so the core's
// ONE generic renewal path drives Codex like every other OAuth bag.
pub mod codex {
    /// Codex CLI's official OAuth client id (a public client — no secret exists).
    pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
    pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
    /// Registered redirect — the reason a server-side callback is impossible.
    pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
    pub const SCOPES: &str = "openid profile email offline_access";
    /// The ChatGPT-internal endpoint the `codex-responses` native driver calls.
    pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
}

// MARK: - Figma OAuth constants
//
// Mafold's OWN registered app (figma.com/developers/apps), unlike `codex`
// above, which borrows a vendor CLI's published client. The difference is
// forced: Figma's authorization server advertises only `client_secret_basic` /
// `client_secret_post`, so no public client can exist, and its MCP dynamic
// registration answers 403 to everyone outside the Figma MCP Catalog
// (re-probed 2026-08-15 — still 403; the catalog is a waitlist, so this stays
// true until Mafold is listed and this whole module can be deleted in favour
// of `mcp_url` alone).
//
// The client id is public by construction — it rides in the authorize URL a
// user can read off their own address bar. The SECRET is never here: it lives
// in `FIGMA_OAUTH_CLIENT_SECRET` on the api host, which is why `TOKEN_ENDPOINT`
// points at mafold-api rather than at Figma.
pub mod figma {
    pub const CLIENT_ID: &str = "klmdBw75sIyOeAnlAiKn8e";
    pub const AUTHORIZE_URL: &str = "https://www.figma.com/oauth";
    /// Mafold's broker, NOT Figma — see [`super::BrokerSpec`].
    pub const TOKEN_ENDPOINT: &str = "https://api.mafold.com/api/exchangeConnectionToken";
    pub const UPSTREAM_TOKEN: &str = "https://api.figma.com/v1/oauth/token";
    /// Figma renews at a DIFFERENT path than it mints. Getting this wrong is a
    /// 404 an hour after linking, not at link time.
    pub const UPSTREAM_REFRESH: &str = "https://api.figma.com/v1/oauth/refresh";
    /// A WEB callback, not a loopback one — and that is the whole point.
    ///
    /// Codex's redirect is `localhost:1455` because OpenAI registered it that
    /// way and we cannot change it; that constraint is what forces a machine of
    /// the user's into the loop. This app is OURS, so the redirect is a page we
    /// serve, and the browser finishes the link alone. **The default user has
    /// no mafold-cli**; a provider that can only be linked from a terminal is a
    /// provider most users cannot link at all.
    ///
    /// This is the canonical one. Every ORIGIN that runs the flow must also be
    /// registered in the Figma app (the dev server's included), because Figma
    /// checks the redirect against its own list — an unregistered origin is
    /// refused at the consent screen, which is the correct place to find out.
    pub const REDIRECT_URI: &str = "https://mafold.com/app/link/callback";
    /// Every scope the Figma app is registered for — the granular successors of
    /// the deprecated `files:read`, plus design-system, dev-resource, project
    /// and webhook access.
    ///
    /// The token's scopes are fixed by what the AUTHORIZE URL asks for, not by
    /// what the app is allowed to ask for: ticking a box in the developer
    /// console widens the ceiling and changes nothing about a grant already
    /// minted. Asking for four of fifteen is why a fully-ticked app still came
    /// back able to do almost nothing.
    ///
    /// NOT here, because Figma does not offer it to a registered app at all:
    /// `mcp:connect`, which `mcp.figma.com` names in its 401 challenge. That
    /// scope belongs to Figma's own MCP authorization server, whose clients are
    /// created by dynamic registration — 403 outside the MCP Catalog waitlist
    /// (re-probed 2026-08-15). Until Mafold is listed, MCP is unreachable and
    /// the REST surface below is the whole capability.
    pub const SCOPES: &str = "current_user:read \
        file_content:read file_metadata:read file_versions:read \
        file_comments:read file_comments:write \
        library_assets:read library_content:read team_library_content:read \
        file_dev_resources:read file_dev_resources:write \
        project_metadata:read projects:read \
        webhooks:read webhooks:write";
    pub const SECRET_ENV: &str = "FIGMA_OAUTH_CLIENT_SECRET";
}

const OAUTH_BAG: &[SecretField] = &[
    SecretField {
        key: "access_token",
        label: "Access token",
        required: true,
        issued: false,
    },
    SecretField {
        key: "refresh_token",
        label: "Refresh token",
        required: false,
        issued: false,
    },
    SecretField {
        key: "expires_at",
        label: "Expires at (unix ms)",
        required: false,
        issued: true,
    },
];

/// [`OAUTH_BAG`] plus the two things a renewal has to spend the refresh token
/// against.
///
/// The distinction is not cosmetic. `OAUTH_BAG` is for a grant we IMPORT whole
/// and never renew ourselves; this one is for a grant WE minted, which means a
/// daemon on some other machine — one that never saw the consent screen — has
/// to be able to renew it from the sealed payload alone. `client_id` and
/// `token_endpoint` travel with the credential for exactly that reason, and are
/// `issued` so no form ever asks a human for them.
const OAUTH_RENEWABLE_BAG: &[SecretField] = &[
    SecretField {
        key: "access_token",
        label: "Access token",
        required: true,
        issued: false,
    },
    SecretField {
        key: "refresh_token",
        label: "Refresh token",
        required: false,
        issued: false,
    },
    SecretField {
        key: "expires_at",
        label: "Expires at (unix ms)",
        required: false,
        issued: true,
    },
    SecretField {
        key: "client_id",
        label: "OAuth client id",
        required: false,
        issued: true,
    },
    SecretField {
        key: "token_endpoint",
        label: "Token endpoint",
        required: false,
        issued: true,
    },
];

const CODEX_BAG: &[SecretField] = &[
    SecretField {
        key: "access_token",
        label: "Access token",
        required: true,
        issued: false,
    },
    SecretField {
        key: "refresh_token",
        label: "Refresh token",
        required: false,
        issued: false,
    },
    SecretField {
        key: "id_token",
        label: "ID token",
        required: false,
        issued: false,
    },
    SecretField {
        key: "account_id",
        label: "Account id",
        required: false,
        issued: false,
    },
    // The renewal trio (see `TOKEN` for why these live in the payload): a
    // laptop daemon must be able to refresh a grant that another device — or
    // `--import` from the Codex CLI's own file — obtained. Codex's client id
    // is a fixed public constant rather than dynamically registered, but
    // storing it per-connection keeps ONE renewal path in the core instead of
    // a codex branch reading a different source.
    SecretField {
        key: "expires_at",
        label: "Expires at (unix ms)",
        required: false,
        issued: true,
    },
    SecretField {
        key: "client_id",
        label: "OAuth client id",
        required: false,
        issued: true,
    },
    SecretField {
        key: "token_endpoint",
        label: "Token endpoint",
        required: false,
        issued: true,
    },
];

const API_KEY: &[SecretField] = &[SecretField {
    key: "api_key",
    label: "API key",
    required: true,
    issued: false,
}];

/// Providers whose credential is a bearer token, however it was obtained: an
/// OAuth grant returns `access_token`, and a hand-pasted integration token is
/// sent in exactly the same header. One field name for both paths — naming it
/// `token` would have made every OAuth grant silently drop its own token when
/// the payload was filtered against this list.
const TOKEN: &[SecretField] = &[
    SecretField {
        key: "access_token",
        label: "Access token",
        required: true,
        issued: false,
    },
    SecretField {
        key: "refresh_token",
        label: "Refresh token",
        required: false,
        issued: false,
    },
    SecretField {
        key: "expires_at",
        label: "Expires at (unix ms)",
        required: false,
        issued: true,
    },
    SecretField {
        key: "client_id",
        label: "OAuth client id",
        required: false,
        issued: true,
    },
    SecretField {
        key: "token_endpoint",
        label: "Token endpoint",
        required: false,
        issued: true,
    },
];

/// Every provider Mafold knows how to hold a credential for.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "claude-code-oauth",
        display: "Claude Code (OAuth)",
        blurb: "Run Claude Code as your agent",
        badge: "claudecode",
        kind: ProviderKind::OAuth,
        fields: OAUTH_BAG,
        import_path: Some(".claude/.credentials.json"),
        env_var: None,
        auth: BEARER,
        oauth_capable: false,
        help_url: None,
        mcp_url: None,
        native_api: None,
        oauth_client: None,
    },
    ProviderSpec {
        id: "anthropic-api",
        display: "Anthropic API Platform",
        blurb: "Your own key for Claude models",
        badge: "claude",
        kind: ProviderKind::ApiKey,
        fields: API_KEY,
        import_path: None,
        env_var: Some("ANTHROPIC_API_KEY"),
        auth: ANTHROPIC_API_KEY,
        oauth_capable: false,
        help_url: Some("https://console.anthropic.com/settings/keys"),
        mcp_url: None,
        native_api: None,
        oauth_client: None,
    },
    ProviderSpec {
        id: "openai-api",
        display: "OpenAI Platform",
        blurb: "Your own key for GPT models",
        badge: "openai",
        kind: ProviderKind::ApiKey,
        fields: API_KEY,
        import_path: None,
        env_var: Some("OPENAI_API_KEY"),
        auth: BEARER_API_KEY,
        oauth_capable: false,
        help_url: Some("https://platform.openai.com/api-keys"),
        mcp_url: None,
        native_api: None,
        oauth_client: None,
    },
    ProviderSpec {
        id: "codex-oauth",
        display: "Codex (OAuth)",
        blurb: "Run Codex as your agent",
        badge: "openai",
        kind: ProviderKind::OAuth,
        fields: CODEX_BAG,
        import_path: Some(".codex/auth.json"),
        env_var: Some("CODEX_API_KEY"),
        auth: BEARER,
        oauth_capable: false,
        help_url: None,
        mcp_url: None,
        native_api: Some("codex-responses"),
        oauth_client: Some(OAuthClientSpec {
            client_id: codex::CLIENT_ID,
            authorize_url: codex::AUTHORIZE_URL,
            token_endpoint: codex::TOKEN_ENDPOINT,
            redirect_uri: codex::REDIRECT_URI,
            scopes: codex::SCOPES,
            // What the Codex CLI itself sends: organizations in the id_token
            // (that is where chatgpt_account_id rides), and the simplified
            // consent screen built for exactly this dance.
            extra_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
            ],
            // A public client: no secret exists to broker.
            broker: None,
        }),
    },
    ProviderSpec {
        id: "notion",
        display: "Notion",
        blurb: "Read and write your workspace",
        badge: "notion",
        kind: ProviderKind::ApiKey,
        fields: TOKEN,
        import_path: None,
        env_var: Some("NOTION_TOKEN"),
        auth: BEARER,
        oauth_capable: true,
        help_url: Some("https://www.notion.so/my-integrations"),
        mcp_url: Some("https://mcp.notion.com/mcp"),
        native_api: None,
        oauth_client: None,
    },
    ProviderSpec {
        id: "figma",
        display: "Figma",
        blurb: "Read files, frames, and designs",
        badge: "figma",
        kind: ProviderKind::ApiKey,
        fields: TOKEN,
        import_path: None,
        env_var: Some("FIGMA_TOKEN"),
        auth: FIGMA_TOKEN_HEADER,
        // Figma runs a real MCP server and a real authorization server, but
        // both doors a browser could walk through are shut: registration is
        // 403, and the token endpoint wants a client secret. So the way in is
        // a personal access token (Figma ▸ Settings ▸ Personal access tokens),
        // pasted once — which its MCP server accepts directly.
        oauth_capable: false,
        help_url: Some("https://www.figma.com/developers/api#access-tokens"),
        mcp_url: Some("https://mcp.figma.com/mcp"),
        native_api: None,
        oauth_client: None,
    },
    // Figma twice, and NOT a duplicate: the two rows hold different
    // credentials that ride differently. A personal access token is `figd_…`
    // in `X-Figma-Token`; an OAuth grant is a bearer token that Figma rejects
    // on that header. `auth` is one AuthStyle per row, so one row cannot carry
    // both — the same reason `codex-oauth` sits beside `openai-api`.
    ProviderSpec {
        id: "figma-oauth",
        display: "Figma (sign in)",
        blurb: "Read files, frames, and designs",
        badge: "figma",
        kind: ProviderKind::OAuth,
        // Renewable, not plain: this grant is minted by whichever device ran the
        // consent screen, and every OTHER device has to renew it later knowing
        // only what the sealed payload says.
        fields: OAUTH_RENEWABLE_BAG,
        import_path: None,
        env_var: None,
        auth: BEARER,
        // False, and it must stay false: this row links through a CONFIDENTIAL
        // client. `oauth_capable` means "a browser alone can do it" — the flag
        // the vault reads to promise the server never sees a token. Brokered
        // linking cannot make that promise, so it must not claim the flag.
        oauth_capable: false,
        help_url: Some("https://www.figma.com/developers/apps"),
        mcp_url: Some("https://mcp.figma.com/mcp"),
        native_api: None,
        oauth_client: Some(OAuthClientSpec {
            client_id: figma::CLIENT_ID,
            authorize_url: figma::AUTHORIZE_URL,
            token_endpoint: figma::TOKEN_ENDPOINT,
            redirect_uri: figma::REDIRECT_URI,
            scopes: figma::SCOPES,
            // Figma's authorization server rejects a request with no `state`
            // (`require_state_parameter: true` in its metadata); the generic
            // dance always sends one, so nothing is needed here.
            extra_params: &[],
            broker: Some(BrokerSpec {
                upstream_token: figma::UPSTREAM_TOKEN,
                upstream_refresh: figma::UPSTREAM_REFRESH,
                secret_env: figma::SECRET_ENV,
            }),
        }),
    },
];

pub fn provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// The registry as a client sees it — display-ready, so a UI renders it
/// VERBATIM and holds no per-provider table of its own.
///
/// This exists because the alternative is every client hand-maintaining a
/// `{id → name, blurb, icon}` map, which is a second registry that drifts from
/// this one and has to be edited in N places to add a provider. `fields` /
/// `import_path` / `env_var` are deliberately absent: they describe how the
/// **cli** collects a secret, and no other surface should grow opinions about
/// that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub display: String,
    pub blurb: String,
    /// Brand-mark slug; empty when the provider has no mark.
    pub badge: String,
    pub kind: ProviderKind,
    /// The fields a form must collect. Enough to RENDER an input per field —
    /// not enough to infer where a secret comes from, which stays cli business.
    ///
    /// Excludes anything the link flow issues; see [`Self::payload_keys`].
    pub fields: Vec<SecretFieldInfo>,
    /// Every key allowed in the sealed payload, including the ones a form never
    /// shows.
    ///
    /// A client filtering a provider's grant must filter against **this**, not
    /// against `fields`. Filtering against `fields` is how `expires_at` was
    /// being thrown away at link time: the OAuth response carried it, the
    /// registry didn't list it as an input, so it vanished — and the failure
    /// showed up an hour later as an expired token nobody could refresh,
    /// nowhere near the code that dropped it.
    pub payload_keys: Vec<String>,
    /// Where the user goes to mint this credential, for providers that are
    /// pasted rather than consented to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_url: Option<String>,
    /// True when a browser can hand the user to the provider's own consent
    /// screen instead of asking them to paste a token.
    ///
    /// Data, not a client-side name check — the browser must not have to know
    /// which providers happen to speak OAuth. Whether the SERVER is configured
    /// for it is a separate question answered at `startConnectionLink`; a
    /// provider that supports OAuth but has no client id there fails with a
    /// message naming the missing variable, which is an operator problem, not
    /// something to hide from the UI by flipping this flag.
    pub oauth: bool,
    /// The provider's MCP server URL, when it has one. A client that sees this
    /// should prefer it: registration is dynamic and the exchange stays in the
    /// browser, so linking needs no server configuration at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_url: Option<String>,
    /// Whether a client with no filesystem (a browser) can complete this
    /// provider on its own.
    ///
    /// DERIVED here, not decided by the client: a provider is browser-linkable
    /// exactly when it has no local file to import. Handing the client a
    /// boolean instead of an `import_path` is the difference between "render
    /// what you're told" and every surface re-deriving the same rule slightly
    /// differently — and the browser has no `$HOME` to check against anyway.
    pub browser_linkable: bool,
    /// One of the user's own **devices** can run this provider's consent screen
    /// end-to-end, so any client — a browser with no filesystem, a phone — links
    /// it with one tap: `startConnectionLink` hands the request to a machine
    /// that holds the vault key, that machine binds the vendor's registered
    /// loopback redirect and answers with the authorize URL, and the credential
    /// is born and sealed there.
    ///
    /// Derived from the registered redirect being a **loopback** address, which
    /// is the only thing that actually forces a machine into the loop: nothing
    /// but a process on that machine can receive `http://localhost:…`.
    ///
    /// It is NOT "has a fixed client". Reading it that way is how Figma briefly
    /// became terminal-only — a vendor app WE registered, whose redirect we
    /// could have pointed at a page we serve, sent to a daemon most users do
    /// not run. A UI reads this instead of naming providers: `browser_linkable`
    /// says "this surface can finish it alone", `device_link` says "ask a
    /// device to". A provider with neither is the only one that needs a
    /// terminal, and there should be as few of those as the vendors allow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub device_link: bool,
    /// The fixed, PUBLIC parameters a browser needs to run this provider's
    /// consent screen itself, when the vendor refuses dynamic registration.
    ///
    /// Every field here is public by construction — the client id rides in an
    /// authorize URL the user can read off their own address bar. The SECRET,
    /// when the vendor demands one, never appears: `token_endpoint` points at
    /// mafold-api's broker, which is the only party that holds it.
    ///
    /// Present ⇒ a browser can link this without any local install. `None` for
    /// providers linked by dynamic registration (they discover all of this at
    /// run time) and for loopback clients (which a browser cannot receive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_fixed: Option<FixedClientInfo>,
    /// How this provider's credential rides on a request.
    ///
    /// Here because CALLING a connection needs exactly three things from the
    /// registry — where to send (`mcp_url`), how to authenticate (this), and
    /// whether a native driver handles it (`native_api`) — and none of the
    /// three is a secret. Serving them is what lets a new provider reach every
    /// client without five app releases.
    pub auth: AuthInfo,
    /// A local file this credential can be lifted from, relative to `$HOME`.
    /// Only a surface with a filesystem can act on it — which is why
    /// `browser_linkable` is derived here rather than leaving every client to
    /// re-decide. Served rather than compiled in for the same reason as the
    /// rest: a provider whose vendor CLI changes where it writes should not
    /// need a cli release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_path: Option<String>,
    /// Environment variable an API key conventionally arrives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
    /// Names a NATIVE driver in the core for providers whose callable surface
    /// is not MCP. Data can name a driver; the driver itself is code, so a pack
    /// naming one this build lacks must say "update the app" rather than
    /// pretend. The only part of a provider that a release still gates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_api: Option<String>,
}

/// The key a published provider pack must be signed with, base64 raw Ed25519.
///
/// Here, not in the core or the api, because signer and both verifiers must
/// agree on one key — and because this is the ONE piece of the registry that is
/// still compiled into clients. It is a key, not a table: adding a provider
/// never touches it, so shipping a provider still needs no app release. Only
/// rotating the publishing identity does.
///
/// The private half exists solely as the `MAFOLD_PROVIDERS_SIGNING_KEY` secret
/// used by `publish-providers.yml`, and on no server that serves packs. An api
/// that was taken over can therefore withhold or replay a pack, but cannot mint
/// one that sends a device's decrypted credential somewhere new.
pub const PACK_PUBLIC_KEY_B64: &str = "5p8gk69DfUBrwm4TYeisbc86ZqfjplSsfSZcL5HVbBU=";

/// A published registry, exactly as it travels and is stored.
///
/// The signature belongs to the pack rather than to the transport, so the same
/// three fields are what CI signs, what the api keeps, and what a client
/// verifies. A server that stored the rows without the signature would have to
/// be trusted to re-attest them, which is the trust this design removes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPack {
    pub version: u32,
    pub providers: Vec<ProviderInfo>,
    /// Base64 raw Ed25519 over [`providers_digest`].
    pub signature: String,
}

/// A content hash of the whole provider pack.
///
/// Same job as [`crate::langpack_checksum`]: let a client ask "is what I cached
/// still what the server has" in one comparison, and let a publish pipeline
/// prove it changed something. Canonicalised by hand for the same reason —
/// `serde_json`'s `preserve_order` is a build-graph-wide flag we do not control.
pub fn providers_checksum(providers: &[ProviderInfo]) -> String {
    crate::fnv1a_hex(providers_canonical(providers).as_bytes())
}

/// The exact bytes every party agrees the pack "is".
///
/// One function because a signature is only worth anything if the signer and
/// the verifier hash the same string down to the last comma; two canonicalisers
/// would produce a pack that verifies nowhere and a bug that looks like key
/// mismatch.
pub fn providers_canonical(providers: &[ProviderInfo]) -> String {
    let value = serde_json::to_value(providers).unwrap_or(serde_json::Value::Null);
    let mut canonical = String::new();
    crate::canon_json(&value, &mut canonical);
    canonical
}

/// What a publish SIGNS: SHA-256 over the version and the canonical pack.
///
/// Deliberately not [`providers_checksum`], which is FNV — fine for "did this
/// change?", useless against someone choosing what it changes to. A pack tells
/// devices where to send decrypted credentials, so the digest under the
/// signature has to be one an attacker cannot aim.
///
/// The version is inside the digest so a valid old signature cannot be replayed
/// over a newer pack number to make a downgrade look current.
pub fn providers_digest(version: u32, providers: &[ProviderInfo]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"mafold-provider-pack-v1\n");
    h.update(version.to_string().as_bytes());
    h.update(b"\n");
    h.update(providers_canonical(providers).as_bytes());
    h.finalize().into()
}

/// [`AuthStyle`], owned — the form that survives a trip through the cloud.
///
/// The `&'static str` version is fine for a table compiled into a binary and
/// useless for one fetched at run time, and these two must not drift: a client
/// that guesses `Authorization` for a provider wanting `X-Figma-Token` sends a
/// credential that is simply refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInfo {
    pub header: String,
    pub prefix: String,
    pub field: String,
}

impl From<AuthStyle> for AuthInfo {
    fn from(a: AuthStyle) -> Self {
        Self { header: a.header.into(), prefix: a.prefix.into(), field: a.field.into() }
    }
}

/// See [`ProviderInfo::oauth_fixed`]. Public parameters only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedClientInfo {
    pub client_id: String,
    pub authorize_url: String,
    /// Where the code→token exchange is POSTed. mafold-api for a brokered
    /// vendor, the vendor itself otherwise.
    pub token_endpoint: String,
    /// The redirect registered at the vendor.
    ///
    /// Carried for LOOPBACK clients, where a terminal has to bind exactly this
    /// port. A browser ignores it and uses its own origin's callback, because
    /// the vendor validates against a list and only the origin actually running
    /// the flow can receive the code.
    pub redirect_uri: String,
    pub scopes: String,
    /// Sent verbatim as extra authorize-URL parameters.
    pub extra_params: Vec<(String, String)>,
}

/// A redirect only a process on the user's own machine can receive.
///
/// The one question that decides whether a link needs a device, asked in one
/// place so no surface answers it differently.
pub fn is_loopback_redirect(uri: &str) -> bool {
    uri.starts_with("http://localhost")
        || uri.starts_with("http://127.0.0.1")
        || uri.starts_with("http://[::1]")
}

/// One field of a provider's secret, as a form needs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFieldInfo {
    pub key: String,
    pub label: String,
    pub required: bool,
}

/// Every provider, display-ready. One JSON blob, one definition.
pub fn provider_infos() -> Vec<ProviderInfo> {
    PROVIDERS
        .iter()
        .map(|p| ProviderInfo {
            id: p.id.to_string(),
            display: p.display.to_string(),
            blurb: p.blurb.to_string(),
            badge: p.badge.to_string(),
            kind: p.kind,
            fields: p
                .fields
                .iter()
                .filter(|f| !f.issued)
                .map(|f| SecretFieldInfo {
                    key: f.key.to_string(),
                    label: f.label.to_string(),
                    required: f.required,
                })
                .collect(),
            payload_keys: p.fields.iter().map(|f| f.key.to_string()).collect(),
            browser_linkable: p.import_path.is_none(),
            device_link: p
                .oauth_client
                .is_some_and(|oc| is_loopback_redirect(oc.redirect_uri)),
            // Two ways a browser finishes a link by itself: dynamic
            // registration (no setup at all — prefer it), or a fixed client
            // whose redirect is a page we serve. Both end with the grant sealed
            // in the browser; neither needs anything installed.
            oauth: p.oauth_capable
                || p.oauth_client
                    .is_some_and(|oc| !is_loopback_redirect(oc.redirect_uri)),
            // Served for EVERY fixed client, loopback included — a terminal
            // needs the registered port, a browser needs the endpoints, and
            // `device_link` already says which of the two can finish the dance.
            // Withholding it from loopback rows is what would leave the cli
            // holding a compiled-in copy of exactly these five strings.
            oauth_fixed: p
                .oauth_client
                .map(|oc| FixedClientInfo {
                    client_id: oc.client_id.to_string(),
                    authorize_url: oc.authorize_url.to_string(),
                    token_endpoint: oc.token_endpoint.to_string(),
                    redirect_uri: oc.redirect_uri.to_string(),
                    scopes: oc.scopes.to_string(),
                    extra_params: oc
                        .extra_params
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                }),
            help_url: p.help_url.map(str::to_string),
            mcp_url: p.mcp_url.map(str::to_string),
            auth: p.auth.into(),
            import_path: p.import_path.map(str::to_string),
            env_var: p.env_var.map(str::to_string),
            native_api: p.native_api.map(str::to_string),
        })
        .collect()
}

// MARK: - Wire model

/// A connection as the **server** knows it: a name, a provider, a label, and
/// bytes it cannot read.
///
/// The three cleartext fields are chosen so `mafold connection list` is useful
/// offline-of-the-key — you can see *that* you linked an Anthropic key ending
/// `3f9a` without any device present. Everything that would let someone act as
/// you lives in `blob`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionMeta {
    pub name: String,
    pub provider: String,
    /// A short human tag for the linked identity — a masked key tail
    /// (`sk-ant-…3f9a`), an account email, a workspace name. Written by the
    /// client that created the connection; never derived server-side, because
    /// deriving it would require reading the secret.
    #[serde(default)]
    pub label: String,
    /// base64 of `nonce(24) || XChaCha20-Poly1305(DEK, secret_json)`.
    pub blob: String,
    /// base64 of the DEK wrapped under the user master key.
    pub wrapped_dek: String,
    /// Which UMK generation wrapped `wrapped_dek`. A device holding an older
    /// generation reports "locked" instead of returning a decrypt failure that
    /// looks like corruption.
    #[serde(default)]
    pub key_id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A device enrolled in the user's vault.
///
/// `public_key` is the only half the server ever holds. `sealed_umk` is the user
/// master key encrypted **to** that public key by an already-unlocked device —
/// the server relays it and cannot open it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultDevice {
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    /// base64 X25519 public key.
    pub public_key: String,
    /// Short digest of `public_key`, for the human comparing screens during
    /// approval. Approval without a fingerprint check is approval of whatever
    /// key the server chose to show you.
    pub fingerprint: String,
    pub approved: bool,
    pub added_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<i64>,
    /// Set once an existing device has wrapped the UMK for this one. Served
    /// only to the device it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_umk: Option<String>,
    #[serde(default)]
    pub key_id: String,
}

/// The offline escape hatch: the UMK wrapped under an Argon2id key derived from
/// a passphrase the user writes down.
///
/// Without this, a user whose only enrolled device dies has no path back —
/// there is no one else who can re-wrap, which is exactly the property we
/// wanted. A recovery blob is how that property stops being a footgun.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultRecovery {
    /// base64 Argon2id salt.
    pub salt: String,
    pub mem_kib: u32,
    pub time_cost: u32,
    pub lanes: u32,
    /// base64 of `nonce(24) || XChaCha20-Poly1305(kdf_key, umk)`.
    pub sealed_umk: String,
    pub key_id: String,
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The providers the layer shipped with must keep their ids: an id is the
    /// stored `provider` string on every existing row, so renaming one silently
    /// strands those connections behind an "unknown provider". Appending is
    /// fine and is why this lists rather than counts — but the append has to be
    /// deliberate enough to edit this line.
    #[test]
    fn provider_ids_are_stable() {
        let ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "claude-code-oauth",
                "anthropic-api",
                "openai-api",
                "codex-oauth",
                "notion",
                "figma",
                "figma-oauth",
            ]
        );
    }

    #[test]
    fn every_provider_has_a_required_field_and_a_way_in() {
        for p in PROVIDERS {
            assert!(
                p.fields.iter().any(|f| f.required),
                "{}: no required field — `add` could store an empty credential",
                p.id
            );
            assert!(
                p.import_path.is_some()
                    || p.env_var.is_some()
                    || p.kind == ProviderKind::ApiKey
                    // A consent screen the cli can drive end-to-end IS a way in,
                    // and the best one: nothing is pasted, imported, or read out
                    // of the environment. Omitting it from this list would make
                    // the healthiest shape of provider the only illegal one.
                    || p.oauth_client.is_some(),
                "{}: nothing to import, no env var, not pasteable, no consent screen",
                p.id
            );
        }
    }

    /// Ids are used in paths and command lines; keep them boring.
    #[test]
    fn provider_ids_are_slugs() {
        for p in PROVIDERS {
            assert!(
                p.id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} is not a slug",
                p.id
            );
            assert!(!p.display.is_empty());
        }
    }

    /// A client renders this verbatim, so an empty field is a blank row in a UI
    /// nobody can fix from the client side. Catch it here instead.
    #[test]
    fn every_provider_is_display_ready() {
        for p in PROVIDERS {
            assert!(!p.display.is_empty(), "{}: no display name", p.id);
            assert!(!p.blurb.is_empty(), "{}: no blurb", p.id);
            assert!(!p.badge.is_empty(), "{}: no badge slug", p.id);
            assert!(
                p.badge
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "{}: badge `{}` must be a bare slug — it becomes /assets/bot/<slug>.png",
                p.id,
                p.badge
            );
        }
    }

    #[test]
    fn provider_infos_carry_the_whole_registry_and_no_secrets_machinery() {
        let infos = provider_infos();
        assert_eq!(infos.len(), PROVIDERS.len());
        let json = serde_json::to_string(&infos).unwrap();
        assert!(json.contains("\"anthropic-api\""));
        assert!(json.contains("Your own key for Claude models"));

        // These two USED to be withheld, to stop every surface growing its own
        // opinion about where a secret comes from. Withholding stopped being
        // the way to enforce that the moment this became the ONLY registry:
        // a cli that cannot read them keeps a compiled-in copy, which is the
        // second source of truth the whole design exists to delete.
        //
        // Neither is a secret — the name of an environment variable and the
        // path a vendor's own CLI writes to are both public. What still stops
        // re-derivation is that the DECISION remains derived here.
        assert!(json.contains("ANTHROPIC_API_KEY"), "the cli needs the env var name");
        assert!(json.contains(".credentials.json"), "the cli needs the import path");
        for info in &infos {
            let spec = provider(&info.id).unwrap();
            assert_eq!(
                info.browser_linkable,
                spec.import_path.is_none(),
                "{}: the surface question stays answered here, not re-derived from import_path",
                info.id
            );
        }
    }

    /// A browser has no `$HOME`, so anything that must be lifted off disk can
    /// only be finished in a terminal. That split is derived from the registry,
    /// so adding an import-based provider can't accidentally offer a web form
    /// that cannot possibly work.
    #[test]
    fn browser_linkable_tracks_whether_a_local_file_is_needed() {
        for info in provider_infos() {
            let spec = provider(&info.id).unwrap();
            assert_eq!(
                info.browser_linkable,
                spec.import_path.is_none(),
                "{} disagrees with its import_path",
                info.id
            );
        }
        let by = |id: &str| provider_infos().into_iter().find(|p| p.id == id).unwrap();
        assert!(by("notion").browser_linkable && by("figma").browser_linkable);
        assert!(!by("claude-code-oauth").browser_linkable);
        assert!(!by("codex-oauth").browser_linkable);
    }

    /// `browser_linkable: false` must not be read as "terminal only". Codex is
    /// the case that matters: no browser can finish it alone (the vendor's
    /// redirect is a loopback port), and yet ONE TAP links it anywhere, because
    /// a device of yours runs the dance. A client decides which of the two
    /// affordances to draw from these booleans and from nothing else — the day
    /// this pair says "neither" for codex is the day the web pane silently
    /// starts telling people to open a terminal again.
    #[test]
    fn device_link_marks_the_providers_a_machine_can_consent_for() {
        for info in provider_infos() {
            let spec = provider(&info.id).unwrap();
            // A device is required exactly when the registered redirect can
            // only be received by a process on that machine. Asserting
            // `oauth_client.is_some()` here instead is what let a web-callback
            // client be routed to a daemon the default user does not run.
            assert_eq!(
                info.device_link,
                spec.oauth_client
                    .is_some_and(|oc| is_loopback_redirect(oc.redirect_uri)),
                "{} disagrees with its redirect",
                info.id
            );
            // The whole point of the change above: a fixed client with a web
            // redirect must leave the browser able to finish it alone.
            if let Some(oc) = spec.oauth_client {
                if !is_loopback_redirect(oc.redirect_uri) {
                    assert!(info.oauth, "{}: web redirect but no browser flow", info.id);
                    assert!(
                        info.oauth_fixed.is_some(),
                        "{}: browser needs the fixed client parameters",
                        info.id
                    );
                }
            }
            assert!(
                info.browser_linkable || info.device_link || spec.import_path.is_some(),
                "{}: a provider a client can neither finish nor delegate is unlinkable",
                info.id
            );
        }
        let by = |id: &str| provider_infos().into_iter().find(|p| p.id == id).unwrap();
        assert!(by("codex-oauth").device_link, "codex links in one tap");
        assert!(!by("notion").device_link, "notion finishes in the browser");
    }

    /// A form can't render an input it has no label for.
    #[test]
    fn every_provider_exposes_its_fields() {
        for info in provider_infos() {
            assert!(!info.fields.is_empty(), "{}: no fields to render", info.id);
            for f in &info.fields {
                assert!(
                    !f.key.is_empty() && !f.label.is_empty(),
                    "{}: unlabelled field",
                    info.id
                );
            }
        }
    }

    /// An MCP server is a callable surface, so anything holding a credential
    /// can drive it — including a browser, since both servers we speak to send
    /// permissive CORS.
    ///
    /// Note what is deliberately NOT asserted: that MCP implies OAuth. Figma
    /// disproves it — real MCP server, real authorization server, and still no
    /// way for a browser to get a token without a client secret. Linking and
    /// calling are separate questions and this file used to conflate them.
    #[test]
    fn an_mcp_provider_is_reachable_from_a_browser() {
        for info in provider_infos() {
            if info.mcp_url.is_some() {
                assert!(
                    info.browser_linkable,
                    "{}: has MCP but isn't browser-linkable",
                    info.id
                );
            }
        }
        let by = |id: &str| provider_infos().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!(by("notion").mcp_url.as_deref(), Some("https://mcp.notion.com/mcp"));
        assert_eq!(by("figma").mcp_url.as_deref(), Some("https://mcp.figma.com/mcp"));
        // The whole reason `auth` is data: these two disagree, and the
        // transport must not learn either name to cope.
        assert!(by("notion").oauth, "Notion registers dynamically — that path works");
        assert!(
            !by("figma").oauth,
            "Figma's registration endpoint 403s; claiming otherwise offers a button that cannot work"
        );
    }

    /// `auth.field` must name a field the provider actually stores, or a call
    /// would send an empty credential and fail as a third-party 401 — the
    /// least debuggable error this layer can produce.
    #[test]
    fn auth_reads_a_field_the_provider_really_has() {
        for p in PROVIDERS {
            assert!(
                p.fields.iter().any(|f| f.key == p.auth.field),
                "{}: auth reads `{}`, which is not one of its fields",
                p.id,
                p.auth.field
            );
            assert!(!p.auth.header.is_empty(), "{}: auth has no header", p.id);
        }
    }

    /// The bug this split exists to prevent, stated as a test: a provider's
    /// grant carries `expires_at`, and filtering it against the *form* fields
    /// silently dropped it.
    #[test]
    fn payload_keys_are_a_superset_of_the_form_fields() {
        for info in provider_infos() {
            for f in &info.fields {
                assert!(
                    info.payload_keys.contains(&f.key),
                    "{}: `{}` is drawn but not storable",
                    info.id,
                    f.key
                );
            }
        }
        let notion = provider_infos().into_iter().find(|p| p.id == "notion").unwrap();
        assert!(notion.payload_keys.contains(&"expires_at".to_string()));
        assert!(notion.payload_keys.contains(&"client_id".to_string()));
        assert!(
            !notion.fields.iter().any(|f| f.key == "client_id"),
            "no form should ask a human to invent an OAuth client id"
        );
    }

    /// A provider **we call** must be able to STORE what a refresh needs —
    /// otherwise the device that logged in is the only one that could ever
    /// renew, and it is usually not the device doing the calling.
    ///
    /// Scoped to callable surfaces (`mcp_url` OR `native_api`) deliberately: we
    /// renew what we drive. `claude-code-oauth` also holds a refresh token, but
    /// it is spent by the vendor's own CLI against a file we merely imported —
    /// reaching in to rotate it would be two things racing over one credential.
    /// `codex-oauth` crossed this line the day it gained a native driver: a
    /// grant we call with is a grant we must be able to renew, and `--oauth`
    /// mints a fresh grant precisely so that renewal races nothing.
    #[test]
    fn a_provider_we_call_can_store_what_a_refresh_needs() {
        for p in PROVIDERS {
            if p.mcp_url.is_none() && p.native_api.is_none() {
                continue;
            }
            let keys: Vec<&str> = p.fields.iter().map(|f| f.key).collect();
            if !keys.contains(&"refresh_token") {
                continue;
            }
            // What spending a refresh token actually takes. `client_id` is the
            // load-bearing one: the browser registers its OAuth client
            // dynamically, so that id exists nowhere else in the world — miss
            // it and only the device that logged in could ever renew.
            for needed in ["expires_at", "client_id", "token_endpoint"] {
                assert!(
                    keys.contains(&needed),
                    "{}: has refresh_token but nowhere to keep `{needed}` — a refresh \
                     would have to guess it",
                    p.id
                );
            }
        }
    }

    /// A provider a user has to PASTE must say where to get the thing.
    ///
    /// Without this the Figma row is a text box labelled "Access token" and no
    /// way to find one — a form that can only be completed by someone who
    /// already knew the answer.
    #[test]
    fn a_pasted_provider_says_where_to_get_the_credential() {
        for p in PROVIDERS {
            let pasted = p.import_path.is_none() && !p.oauth_capable;
            if pasted {
                assert!(
                    p.help_url.is_some(),
                    "{}: must be pasted but points nowhere",
                    p.id
                );
            }
        }
        let figma = provider("figma").unwrap();
        assert!(figma.help_url.unwrap().contains("figma.com"));
    }

    /// Pinned because it is counter-intuitive and a well-meaning cleanup would
    /// "fix" it back to Bearer: Figma's MCP server rejects `Authorization` for
    /// `figd_` tokens and names the header it wants instead.
    #[test]
    fn figma_does_not_use_bearer() {
        let figma = provider("figma").unwrap();
        assert_eq!(figma.auth.header, "X-Figma-Token");
        assert_eq!(figma.auth.prefix, "");
        assert_eq!(provider("notion").unwrap().auth, BEARER);
    }

    /// An MCP url must be the SERVER endpoint, not a well-known path — the
    /// client derives discovery URLs from it and would double up otherwise.
    #[test]
    fn mcp_urls_are_endpoints_not_discovery_paths() {
        for p in PROVIDERS {
            if let Some(u) = p.mcp_url {
                assert!(u.starts_with("https://"), "{}: {u}", p.id);
                assert!(!u.contains("/.well-known/"), "{}: {u} is a discovery path", p.id);
            }
        }
    }

    #[test]
    fn lookup_finds_and_rejects() {
        assert_eq!(provider("figma").map(|p| p.display), Some("Figma"));
        assert!(
            provider("Figma").is_none(),
            "lookup must be exact — ids are canonical lowercase"
        );
        assert!(provider("dropbox").is_none());
    }

    /// The codex row is the one native (non-MCP) callable surface, and the
    /// contract has three parts a regression would break separately: the driver
    /// name the core dispatches on, the renewal trio the link flows must fill,
    /// and the fact that `client_id`/`token_endpoint` never render as inputs —
    /// they are the CLI's own constants, not something a human should type.
    #[test]
    fn codex_is_natively_callable_and_renewable() {
        let p = provider("codex-oauth").unwrap();
        assert_eq!(p.native_api, Some("codex-responses"));
        let keys: Vec<&str> = p.fields.iter().map(|f| f.key).collect();
        for k in ["access_token", "refresh_token", "account_id", "expires_at", "client_id", "token_endpoint"] {
            assert!(keys.contains(&k), "codex payload must store `{k}`");
        }
        let info = provider_infos().into_iter().find(|i| i.id == "codex-oauth").unwrap();
        assert!(
            !info.fields.iter().any(|f| f.key == "client_id" || f.key == "token_endpoint" || f.key == "expires_at"),
            "issued fields must not become form inputs"
        );
        // The constants the link flows copy into the payload — a typo here
        // strands every new connection at the token endpoint.
        assert!(codex::TOKEN_ENDPOINT.starts_with("https://auth.openai.com/"));
        assert!(codex::REDIRECT_URI.starts_with("http://localhost:1455/"));
        assert!(codex::RESPONSES_URL.ends_with("/backend-api/codex/responses"));
    }

    /// A served descriptor must carry everything a CALL needs, because once the
    /// pack is the only copy a client has, anything missing here is not
    /// recoverable from a compiled-in table.
    #[test]
    fn provider_infos_carry_what_a_call_needs() {
        for info in provider_infos() {
            assert!(!info.auth.header.is_empty(), "{}: no auth header", info.id);
            assert!(!info.auth.field.is_empty(), "{}: no credential field", info.id);
            // The field names where the credential sits IN THE SEALED PAYLOAD.
            // Naming one the payload can never hold is a call that fails with an
            // empty token — a 401 that looks like the user's fault.
            assert!(
                info.payload_keys.contains(&info.auth.field),
                "{}: auth reads `{}`, which is not one of {:?}",
                info.id,
                info.auth.field,
                info.payload_keys
            );
        }
    }

    #[test]
    fn providers_checksum_notices_a_changed_endpoint() {
        let base = provider_infos();
        let sum = providers_checksum(&base);
        assert_eq!(sum, providers_checksum(&provider_infos()), "same input, same digest");

        // The attack the signature exists to stop, as a unit test: repointing a
        // provider must be visible as a different pack.
        let mut tampered = base.clone();
        tampered[0].mcp_url = Some("https://evil.example/mcp".into());
        assert_ne!(sum, providers_checksum(&tampered));

        // Swapping the header a credential rides on is the quieter half of the
        // same attack: same endpoint, but the token moves to somewhere the
        // attacker's proxy reads. It must be just as visible.
        let mut rehomed = base.clone();
        assert_ne!(rehomed[0].auth.header, "X-Exfiltrate", "pick a header it does not already use");
        rehomed[0].auth.header = "X-Exfiltrate".into();
        assert_ne!(sum, providers_checksum(&rehomed), "an auth swap must change the digest too");
    }

    /// `client_id` is the only key the broker has to find a row by, because the
    /// wire body it receives is the vendor's own token-request form and carries
    /// no provider name. Two rows sharing one would route a secret by luck.
    #[test]
    fn oauth_client_ids_are_unique() {
        let ids: Vec<&str> = PROVIDERS.iter().filter_map(|p| p.oauth_client.map(|o| o.client_id)).collect();
        for (i, a) in ids.iter().enumerate() {
            assert!(!ids[i + 1..].contains(a), "two providers share client_id `{a}`");
        }
    }

    #[test]
    fn brokered_rows_point_the_device_at_us_and_name_a_secret() {
        for p in PROVIDERS {
            let Some(oc) = p.oauth_client else { continue };
            let Some(b) = oc.broker else { continue };
            assert!(!b.secret_env.is_empty(), "{}: a brokered row must name its secret env", p.id);
            for u in [b.upstream_token, b.upstream_refresh] {
                assert!(u.starts_with("https://"), "{}: brokered upstream `{u}` must be absolute https", p.id);
            }
            // The whole point: the device posts to the broker, never straight to
            // the vendor — a vendor that wanted no secret would not be brokered.
            assert_ne!(oc.token_endpoint, b.upstream_token, "{}: token_endpoint must be the broker", p.id);
            // Brokering means the server briefly holds a token, which is exactly
            // what `oauth_capable` promises it does not. The two must never both
            // be set, or a client would offer a browser-only link that lies.
            assert!(!p.oauth_capable, "{}: a brokered provider cannot claim oauth_capable", p.id);
        }
    }

    /// Figma is two rows on purpose. Collapsing them looks like cleanup and is
    /// a bug: the credentials ride on different headers.
    #[test]
    fn figma_pat_and_oauth_are_separate_rows_with_different_auth() {
        let pat = provider("figma").unwrap();
        let oauth = provider("figma-oauth").unwrap();
        assert_eq!(pat.auth.header, "X-Figma-Token");
        assert_eq!(oauth.auth.header, "Authorization");
        assert_eq!(pat.badge, oauth.badge, "one brand, one mark");

        let b = oauth.oauth_client.unwrap().broker.unwrap();
        assert_eq!(b.upstream_token, "https://api.figma.com/v1/oauth/token");
        // Figma renews at a different path than it mints; a copy-paste here is a
        // 404 an hour after linking, not at link time.
        assert_eq!(b.upstream_refresh, "https://api.figma.com/v1/oauth/refresh");
        assert_ne!(b.upstream_token, b.upstream_refresh);

        // The scopes a grant gets are the ones the AUTHORIZE URL asks for. A
        // fully-ticked app whose authorize URL asks for four of fifteen mints a
        // token that can do four things — which is exactly how this shipped, and
        // is invisible until a call 403s. So the ask is pinned here.
        let scopes: Vec<&str> = figma::SCOPES.split(' ').collect();
        for want in [
            "current_user:read",
            "file_content:read",
            "file_metadata:read",
            "file_versions:read",
            "file_comments:read",
            "library_content:read",
            "team_library_content:read",
            "file_dev_resources:read",
            "projects:read",
        ] {
            assert!(scopes.contains(&want), "figma scopes must include {want}");
        }
        // The `\` line continuations in the literal are load-bearing: a stray
        // newline or double space rides into the authorize URL verbatim.
        assert!(
            !figma::SCOPES.contains("  ") && !figma::SCOPES.contains('\n'),
            "scope list is a single space-separated line"
        );
        // Not requestable by a registered app — see the constant's note.
        assert!(!figma::SCOPES.contains("mcp:"), "MCP scopes come from Figma's own DCR, not from us");

        // The default user has NO mafold-cli. Figma must therefore be linkable
        // by the browser alone: our own app, our own redirect, a page we serve.
        let info = provider_infos().into_iter().find(|i| i.id == "figma-oauth").unwrap();
        assert!(!info.device_link, "figma-oauth must NOT require a local daemon");
        assert!(info.oauth, "figma-oauth must offer a browser consent flow");
        assert!(info.browser_linkable, "nothing to import from disk");
        let fixed = info.oauth_fixed.expect("browser needs the fixed client");
        assert!(fixed.token_endpoint.contains("exchangeConnectionToken"), "exchange goes through the broker");
        assert!(!fixed.client_id.is_empty() && fixed.authorize_url.starts_with("https://"));
        assert!(!is_loopback_redirect(oauth.oauth_client.unwrap().redirect_uri));
    }
}
