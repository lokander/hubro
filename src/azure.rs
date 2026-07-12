//! Microsoft Entra ID (Azure AD) token acquisition for Azure Database for
//! PostgreSQL (FRE-43).
//!
//! Azure Postgres can be configured for Entra-only login: instead of a
//! password you pass a short-lived **access token** (for the resource
//! `https://ossrdbms-aad.database.windows.net`) as the connection password,
//! with the Postgres username being the Entra principal. This module acquires
//! that token two ways:
//!
//! - **Interactive** — OAuth 2.0 authorization-code + PKCE with a `127.0.0.1`
//!   loopback redirect; opens the user's browser to sign in. A cached refresh
//!   token (persisted by the caller) is redeemed silently first, so the browser
//!   only opens on first sign-in or once the refresh token lapses.
//! - **Managed identity** — the Azure Instance Metadata Service (IMDS), or the
//!   `IDENTITY_ENDPOINT` used by App Service / Container Apps. Only works inside
//!   Azure; always silent.
//!
//! The token never touches the driver directly: the caller splices it in as the
//! password via [`crate::db::url_with_password`]. Only the refresh token is
//! worth persisting (in the OS keyring); access tokens are always re-acquired.

use std::fmt;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// The Azure Database for PostgreSQL OAuth resource; tokens are requested for
/// its `/.default` scope.
pub const OSSRDBMS_RESOURCE: &str = "https://ossrdbms-aad.database.windows.net";

/// The Azure CLI's public client id. It has the `http://localhost` loopback
/// redirect registered and is pre-consented in most tenants, so it works out of
/// the box; users whose tenant disallows it can supply their own app
/// registration's client id instead.
pub const AZURE_CLI_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";

/// How long to wait for the interactive sign-in redirect before giving up.
pub const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(180);

/// How dataview authenticates a Postgres connection to Entra. Persisted with
/// the saved connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum EntraAuth {
    /// Interactive browser sign-in (authorization-code + PKCE). `tenant` is the
    /// directory (tenant id or domain, or `organizations`/`common`); `client_id`
    /// overrides [`AZURE_CLI_CLIENT_ID`].
    Interactive {
        tenant: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// A managed identity (only inside Azure). `client_id` selects a
    /// user-assigned identity; omitted means the system-assigned one.
    ManagedIdentity {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
}

impl EntraAuth {
    /// The default interactive mode: sign in against the `organizations`
    /// authority with the built-in client id.
    pub fn interactive_default() -> Self {
        EntraAuth::Interactive {
            tenant: "organizations".to_string(),
            client_id: None,
        }
    }

    /// Whether a token can be acquired without opening a browser — used by the
    /// session-restore gate so startup never pops a sign-in window. Managed
    /// identity is always silent; interactive is silent only when a cached
    /// refresh token is available to redeem.
    pub fn can_acquire_silently(&self, has_cached_refresh: bool) -> bool {
        match self {
            EntraAuth::ManagedIdentity { .. } => true,
            EntraAuth::Interactive { .. } => has_cached_refresh,
        }
    }
}

/// An acquired access token plus (for interactive auth) a refresh token to
/// persist for silent renewals.
#[derive(Debug, Clone)]
pub struct AccessToken {
    /// The bearer token — spliced in as the Postgres password.
    pub secret: String,
    /// When the token expires (best-effort; used only to decide re-acquisition).
    pub expires_at: SystemTime,
    /// A refresh token to store for silent renewal, when the flow returned one.
    pub refresh_token: Option<String>,
}

/// Where to reach Azure. Real values by default; tests point these at a local
/// fake server so the whole flow runs without Azure.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// Base of the Entra authority, e.g. `https://login.microsoftonline.com`.
    pub login: String,
    /// Base of IMDS, e.g. `http://169.254.169.254`.
    pub imds: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            login: "https://login.microsoftonline.com".to_string(),
            imds: "http://169.254.169.254".to_string(),
        }
    }
}

/// Failure acquiring a token. Every message is user-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AzureError {
    /// Bad configuration (empty tenant, unparseable endpoint, …).
    Config(String),
    /// Network / HTTP transport failure.
    Http(String),
    /// The identity provider returned an OAuth error.
    OAuth { error: String, description: String },
    /// The interactive sign-in did not complete in time.
    Timeout,
    /// Could not launch the browser for interactive sign-in.
    Browser(String),
}

impl fmt::Display for AzureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AzureError::Config(m) => write!(f, "Entra sign-in: {m}"),
            AzureError::Http(m) => write!(f, "Entra sign-in: network error: {m}"),
            AzureError::OAuth { error, description } => {
                if description.is_empty() {
                    write!(f, "Entra sign-in: {error}")
                } else {
                    write!(f, "Entra sign-in: {error}: {description}")
                }
            }
            AzureError::Timeout => write!(f, "Entra sign-in: timed out waiting for the browser"),
            AzureError::Browser(m) => write!(f, "Entra sign-in: could not open a browser: {m}"),
        }
    }
}

impl std::error::Error for AzureError {}

/// Acquires an access token for `auth`. `cached_refresh` is an optional refresh
/// token (from the keyring) to redeem silently before falling back to
/// interactive sign-in. `open_browser` launches the sign-in URL — injected so
/// tests never open a real browser; production passes a `webbrowser::open`
/// wrapper. The scope is always [`OSSRDBMS_RESOURCE`]'s `/.default`.
pub async fn acquire_token(
    auth: &EntraAuth,
    cached_refresh: Option<&str>,
    endpoints: &Endpoints,
    timeout: Duration,
    open_browser: impl FnOnce(&str) -> Result<(), AzureError>,
) -> Result<AccessToken, AzureError> {
    match auth {
        EntraAuth::ManagedIdentity { client_id } => {
            acquire_managed_identity(client_id.as_deref(), endpoints).await
        }
        EntraAuth::Interactive { tenant, client_id } => {
            if tenant.trim().is_empty() {
                return Err(AzureError::Config(
                    "the tenant must not be empty".to_string(),
                ));
            }
            let client_id = client_id.as_deref().unwrap_or(AZURE_CLI_CLIENT_ID);
            // Silent renewal first: a valid refresh token skips the browser.
            if let Some(refresh) = cached_refresh {
                if let Ok(token) = redeem_refresh_token(endpoints, tenant, client_id, refresh).await
                {
                    return Ok(token);
                }
            }
            acquire_interactive(endpoints, tenant, client_id, timeout, open_browser).await
        }
    }
}

/// The `/.default` scope for the Postgres resource, plus `offline_access` so the
/// token endpoint returns a refresh token.
fn scope() -> String {
    format!("{OSSRDBMS_RESOURCE}/.default offline_access")
}

/// Builds the authorization-code request URL.
fn authorize_url(
    login: &str,
    tenant: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &scope())
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("prompt", "select_account")
        .finish();
    format!("{login}/{tenant}/oauth2/v2.0/authorize?{query}")
}

/// Runs the interactive authorization-code + PKCE flow: opens the browser,
/// waits for the loopback redirect, and exchanges the code for a token.
async fn acquire_interactive(
    endpoints: &Endpoints,
    tenant: &str,
    client_id: &str,
    timeout: Duration,
    open_browser: impl FnOnce(&str) -> Result<(), AzureError>,
) -> Result<AccessToken, AzureError> {
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = random_token(24);

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| AzureError::Http(format!("binding a local port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| AzureError::Http(format!("reading the local port: {e}")))?
        .port();
    // 127.0.0.1 (not `localhost`) to match the bind exactly and follow
    // Microsoft's loopback-redirect guidance (avoids a `::1` vs `127.0.0.1`
    // mismatch on dual-stack hosts).
    let redirect_uri = format!("http://127.0.0.1:{port}/");

    let url = authorize_url(
        &endpoints.login,
        tenant,
        client_id,
        &redirect_uri,
        &challenge,
        &state,
    );
    open_browser(&url)?;

    let code = wait_for_redirect(listener, &state, timeout).await?;
    exchange_code(
        endpoints,
        tenant,
        client_id,
        &code,
        &verifier,
        &redirect_uri,
    )
    .await
}

/// Accepts the single loopback redirect, returns a small "done" page to the
/// browser, validates `state`, and yields the authorization `code`.
async fn wait_for_redirect(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AzureError> {
    let (mut stream, _) = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| AzureError::Timeout)?
        .map_err(|e| AzureError::Http(format!("accepting the redirect: {e}")))?;

    // Read until the request line (ends at the first CRLF) is complete — a long
    // authorization code could span more than one TCP segment. Capped so a
    // rogue client can't stream forever.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            buf.truncate(pos);
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(AzureError::Http("redirect request too large".to_string()));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| AzureError::Http(format!("reading the redirect: {e}")))?;
        if n == 0 {
            break; // connection closed before a full request line
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let request_line = String::from_utf8_lossy(&buf);
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let target = target.as_str();

    let result = parse_redirect(target, expected_state);
    // Always answer the browser, whether the redirect was good or not.
    let body = match &result {
        Ok(_) => {
            "<html><body style=\"font-family:sans-serif\"><h3>Signed in.</h3>\
                  <p>You can close this window and return to dataview.</p></body></html>"
        }
        Err(_) => {
            "<html><body style=\"font-family:sans-serif\"><h3>Sign-in failed.</h3>\
                   <p>Return to dataview for details.</p></body></html>"
        }
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    result
}

/// Parses the redirect's request target (`/?code=…&state=…` or an `error`
/// response), validating `state`.
fn parse_redirect(target: &str, expected_state: &str) -> Result<String, AzureError> {
    let parsed =
        url::Url::parse(&format!("http://localhost{target}")).map_err(|e| AzureError::OAuth {
            error: "invalid_redirect".to_string(),
            description: e.to_string(),
        })?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(AzureError::OAuth {
            error,
            description: description.unwrap_or_default(),
        });
    }
    // Reject a mismatched/absent state before trusting the code (CSRF guard).
    if state.as_deref() != Some(expected_state) {
        return Err(AzureError::OAuth {
            error: "state_mismatch".to_string(),
            description: "the sign-in response did not match this request".to_string(),
        });
    }
    code.ok_or_else(|| AzureError::OAuth {
        error: "no_code".to_string(),
        description: "the sign-in response carried no authorization code".to_string(),
    })
}

/// Exchanges an authorization code for tokens at the token endpoint.
async fn exchange_code(
    endpoints: &Endpoints,
    tenant: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<AccessToken, AzureError> {
    let form = [
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("scope", &scope()),
    ];
    post_token(endpoints, tenant, &form).await
}

/// Redeems a refresh token for a fresh access (and refresh) token.
async fn redeem_refresh_token(
    endpoints: &Endpoints,
    tenant: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<AccessToken, AzureError> {
    let form = [
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", &scope()),
    ];
    post_token(endpoints, tenant, &form).await
}

/// POSTs a form to the token endpoint and decodes the token (or OAuth error).
async fn post_token(
    endpoints: &Endpoints,
    tenant: &str,
    form: &[(&str, &str)],
) -> Result<AccessToken, AzureError> {
    let url = format!("{}/{}/oauth2/v2.0/token", endpoints.login, tenant);
    let response = http_client()?
        .post(&url)
        .form(form)
        .send()
        .await
        .map_err(|e| AzureError::Http(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| AzureError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(oauth_error(&body, status.as_u16()));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| AzureError::Http(format!("bad token response: {e}")))?;
    Ok(AccessToken {
        secret: parsed.access_token,
        expires_at: SystemTime::now()
            + Duration::from_secs(parsed.expires_in.unwrap_or(3600).min(24 * 3600)),
        refresh_token: parsed.refresh_token,
    })
}

/// Acquires a token from a managed identity: the App Service `IDENTITY_ENDPOINT`
/// when present, otherwise IMDS.
async fn acquire_managed_identity(
    client_id: Option<&str>,
    endpoints: &Endpoints,
) -> Result<AccessToken, AzureError> {
    // App Service / Container Apps expose a per-instance endpoint + header
    // instead of the shared IMDS address.
    if let (Ok(endpoint), Ok(header)) = (
        std::env::var("IDENTITY_ENDPOINT"),
        std::env::var("IDENTITY_HEADER"),
    ) {
        return imds_get(
            &endpoint,
            "2019-08-01",
            Some(("X-IDENTITY-HEADER", &header)),
            client_id,
        )
        .await;
    }
    let endpoint = format!("{}/metadata/identity/oauth2/token", endpoints.imds);
    imds_get(
        &endpoint,
        "2018-02-01",
        Some(("Metadata", "true")),
        client_id,
    )
    .await
}

/// Performs the metadata-endpoint GET (IMDS or App Service) and decodes the
/// token. There is no refresh token for a managed identity.
async fn imds_get(
    endpoint: &str,
    api_version: &str,
    header: Option<(&str, &str)>,
    client_id: Option<&str>,
) -> Result<AccessToken, AzureError> {
    let mut query = vec![
        ("api-version", api_version.to_string()),
        ("resource", OSSRDBMS_RESOURCE.to_string()),
    ];
    if let Some(client_id) = client_id {
        query.push(("client_id", client_id.to_string()));
    }
    let mut request = http_client()?.get(endpoint).query(&query);
    if let Some((name, value)) = header {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|e| {
        AzureError::Http(format!(
            "{e} (managed identity is only available inside Azure)"
        ))
    })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| AzureError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(oauth_error(&body, status.as_u16()));
    }
    let parsed: ImdsTokenResponse = serde_json::from_str(&body)
        .map_err(|e| AzureError::Http(format!("bad IMDS response: {e}")))?;
    let expires_at = parsed
        .expires_on
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|epoch| SystemTime::UNIX_EPOCH + Duration::from_secs(epoch))
        .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(3600));
    Ok(AccessToken {
        secret: parsed.access_token,
        expires_at,
        refresh_token: None,
    })
}

/// Maps an error response body to [`AzureError::OAuth`], falling back to the raw
/// body when it isn't the standard `{error, error_description}` shape.
fn oauth_error(body: &str, status: u16) -> AzureError {
    match serde_json::from_str::<OAuthErrorResponse>(body) {
        Ok(parsed) => AzureError::OAuth {
            error: parsed.error,
            description: parsed.error_description.unwrap_or_default(),
        },
        Err(_) => AzureError::OAuth {
            error: format!("HTTP {status}"),
            description: body.chars().take(300).collect(),
        },
    }
}

fn http_client() -> Result<reqwest::Client, AzureError> {
    reqwest::Client::builder()
        // Bound every token/IMDS call so a hung endpoint (or a managed-identity
        // probe outside Azure) can't block the connect indefinitely.
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| AzureError::Http(e.to_string()))
}

/// A high-entropy PKCE `code_verifier` (43 chars, base64url of 32 random bytes).
fn pkce_verifier() -> String {
    random_token(32)
}

/// The S256 `code_challenge` for a verifier: base64url(SHA-256(verifier)).
fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// base64url (no padding) of `bytes` random bytes — used for the PKCE verifier
/// and the CSRF `state`. Both are opaque tokens, so any URL-safe encoding works.
fn random_token(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    getrandom::getrandom(&mut raw).expect("OS randomness unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct ImdsTokenResponse {
    access_token: String,
    /// Absolute expiry as a unix-epoch-seconds string (IMDS convention).
    #[serde(default)]
    expires_on: Option<String>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entra_auth_serializes_tagged_and_omits_empty_client_id() {
        let interactive = EntraAuth::Interactive {
            tenant: "contoso.onmicrosoft.com".to_string(),
            client_id: None,
        };
        let text = toml::to_string(&interactive).unwrap();
        assert!(text.contains("method = \"interactive\""));
        assert!(text.contains("tenant = \"contoso.onmicrosoft.com\""));
        assert!(
            !text.contains("client_id"),
            "None client_id is skipped: {text}"
        );
        assert_eq!(toml::from_str::<EntraAuth>(&text).unwrap(), interactive);

        let mi = EntraAuth::ManagedIdentity {
            client_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
        };
        let text = toml::to_string(&mi).unwrap();
        assert!(text.contains("method = \"managedidentity\""));
        assert_eq!(toml::from_str::<EntraAuth>(&text).unwrap(), mi);
    }

    #[test]
    fn silent_capability_depends_on_mode_and_cache() {
        let mi = EntraAuth::ManagedIdentity { client_id: None };
        assert!(mi.can_acquire_silently(false));
        assert!(mi.can_acquire_silently(true));

        let interactive = EntraAuth::interactive_default();
        assert!(!interactive.can_acquire_silently(false));
        assert!(interactive.can_acquire_silently(true));
    }

    #[test]
    fn pkce_challenge_is_the_s256_of_the_verifier() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_are_url_safe_and_high_entropy() {
        let v = pkce_verifier();
        // 32 bytes → 43 base64url chars (no padding), all in the unreserved set.
        assert_eq!(v.len(), 43);
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(pkce_verifier(), pkce_verifier(), "each verifier is fresh");
    }

    #[test]
    fn authorize_url_carries_the_pkce_and_oauth_params() {
        let url = authorize_url(
            "https://login.microsoftonline.com",
            "organizations",
            AZURE_CLI_CLIENT_ID,
            "http://localhost:12345/",
            "CHALLENGE",
            "STATE",
        );
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), "/organizations/oauth2/v2.0/authorize");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], AZURE_CLI_CLIENT_ID);
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["code_challenge"], "CHALLENGE");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "STATE");
        assert_eq!(pairs["redirect_uri"], "http://localhost:12345/");
        assert!(pairs["scope"].contains("ossrdbms-aad.database.windows.net/.default"));
        assert!(pairs["scope"].contains("offline_access"));
    }

    #[test]
    fn redirect_parsing_validates_state_and_extracts_the_code() {
        assert_eq!(
            parse_redirect("/?code=AUTH_CODE&state=S", "S").unwrap(),
            "AUTH_CODE"
        );
        // Wrong state is rejected even with a code present (CSRF guard).
        assert!(matches!(
            parse_redirect("/?code=AUTH_CODE&state=OTHER", "S"),
            Err(AzureError::OAuth { error, .. }) if error == "state_mismatch"
        ));
        // Missing state.
        assert!(matches!(
            parse_redirect("/?code=AUTH_CODE", "S"),
            Err(AzureError::OAuth { error, .. }) if error == "state_mismatch"
        ));
        // An error response surfaces the provider's error, before the state check.
        assert!(matches!(
            parse_redirect("/?error=access_denied&error_description=nope&state=S", "S"),
            Err(AzureError::OAuth { error, description }) if error == "access_denied" && description == "nope"
        ));
    }

    #[test]
    fn error_body_maps_to_oauth_error_with_a_raw_fallback() {
        let structured = oauth_error(
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
            400,
        );
        assert!(matches!(
            structured,
            AzureError::OAuth { error, description } if error == "invalid_grant" && description == "expired"
        ));
        let raw = oauth_error("not json", 500);
        assert!(matches!(
            raw,
            AzureError::OAuth { error, .. } if error == "HTTP 500"
        ));
    }
}
