//! Google OAuth 2.0 for the calendar plugin (native-app flow).
//!
//! - PKCE (S256) authorization against `accounts.google.com`.
//! - Loopback redirect: a `tiny_http` server on `127.0.0.1:{ephemeral port}`
//!   catches the consent-screen redirect (RFC 8252 — Google accepts any
//!   dynamic port for Desktop-type OAuth clients).
//! - Client secret + tokens live as one JSON blob in the OS keychain
//!   (service `dev.fl.companion`, account `calendar:google`), matching how
//!   the host stores API keys.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/calendar.readonly",
];

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const KEYCHAIN_SERVICE: &str = "dev.fl.companion";
const KEYCHAIN_ACCOUNT: &str = "calendar:google";
/// How long the loopback server waits for the consent redirect.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Access + refresh tokens, refreshed lazily by the sync engine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Epoch millis at which the access token expires (with margin applied).
    pub expires_at: i64,
}

/// Everything secret for this integration, as one keychain blob.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyringBlob {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

fn keychain_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("keychain unavailable: {e}"))
}

pub fn load_blob() -> KeyringBlob {
    let entry = match keychain_entry() {
        Ok(e) => e,
        Err(_) => return KeyringBlob::default(),
    };
    let raw = match entry.get_password() {
        Ok(raw) => raw,
        Err(_) => return KeyringBlob::default(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_blob(blob: &KeyringBlob) -> Result<(), String> {
    let entry = keychain_entry()?;
    let raw = serde_json::to_string(blob).map_err(|e| e.to_string())?;
    entry
        .set_password(&raw)
        .map_err(|e| format!("keychain write failed: {e}"))
}

pub fn clear_blob() -> Result<(), String> {
    let entry = keychain_entry()?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        // No entry present is already "cleared".
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keychain delete failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// PKCE + authorization URL
// ---------------------------------------------------------------------------

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    B64URL.encode(buf)
}

/// Returns `(verifier, challenge)` per RFC 7636 S256.
pub fn pkce_pair() -> (String, String) {
    let verifier = random_b64(64);
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, B64URL.encode(digest))
}

pub fn build_auth_url(
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    // access_type=offline + prompt=consent: always get a refresh token back,
    // even when this client was authorized before.
    format!(
        "{AUTH_URL}?response_type=code&client_id={cid}&redirect_uri={redirect}&scope={scopes}&state={state}&code_challenge={challenge}&code_challenge_method=S256&access_type=offline&prompt=consent",
        cid = urlencode(client_id),
        redirect = urlencode(redirect_uri),
        scopes = urlencode(&SCOPES.join(" ")),
        state = urlencode(state),
        challenge = urlencode(challenge),
    )
}

/// Minimal percent-encoding for URL query values (safe set: unreserved chars).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Loopback callback server
// ---------------------------------------------------------------------------

/// Increments on every `google.connect`; an in-flight callback thread for a
/// previous attempt exits when it sees its generation superseded.
pub type ConnectGeneration = Arc<AtomicUsize>;

pub struct ConnectServer {
    /// `http://127.0.0.1:{port}` — pass this as the OAuth redirect_uri.
    pub redirect_uri: String,
    pub verifier: String,
    pub challenge: String,
    pub state: String,
    /// Identity of this attempt; a newer `ConnectServer::new` supersedes it.
    pub generation: usize,
    counter: ConnectGeneration,
    listener: TcpListener,
}

impl ConnectServer {
    /// Bind a fresh loopback listener on an ephemeral port.
    pub fn new(counter: ConnectGeneration) -> Result<ConnectServer, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("could not bind OAuth callback port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| e.to_string())?
            .port();
        let generation = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let (verifier, challenge) = pkce_pair();
        let state = random_b64(24);
        Ok(ConnectServer {
            redirect_uri: format!("http://127.0.0.1:{port}"),
            verifier,
            challenge,
            state,
            generation,
            counter,
            listener,
        })
    }

    /// The consent-screen URL to open in the user's browser.
    pub fn auth_url(&self, client_id: &str) -> String {
        build_auth_url(client_id, &self.redirect_uri, &self.challenge, &self.state)
    }

    /// Block until the browser redirects back (or timeout / superseded).
    /// Runs on its own thread; returns the authorization code.
    pub fn wait_for_code(&self) -> Result<String, String> {
        let listener = self
            .listener
            .try_clone()
            .map_err(|e| format!("OAuth callback server: {e}"))?;
        let server = tiny_http::Server::from_listener(listener, None)
            .map_err(|e| format!("OAuth callback server: {e}"))?;
        let deadline = Instant::now() + CALLBACK_TIMEOUT;
        loop {
            if self.counter.load(Ordering::SeqCst) != self.generation {
                return Err("superseded by a newer sign-in attempt".into());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for the Google sign-in".into());
            }
            let request = match server.try_recv() {
                Ok(Some(req)) => req,
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(format!("OAuth callback server error: {e}")),
            };
            let url = request.url().to_string();
            let mut method_err: Option<String> = None;
            let mut handled = false;
            if url.starts_with("/?") {
                let mut code: Option<String> = None;
                let mut got_state: Option<String> = None;
                let mut oauth_err: Option<String> = None;
                for pair in url[2..].split('&') {
                    let (k, v) = match pair.split_once('=') {
                        Some((k, v)) => (k, v),
                        None => continue,
                    };
                    match k {
                        "code" => code = Some(urldecode(v)),
                        "state" => got_state = Some(urldecode(v)),
                        "error" => oauth_err = Some(urldecode(v)),
                        _ => {}
                    }
                }
                if let Some(err) = oauth_err {
                    method_err = Some(match err.as_str() {
                        "access_denied" => "sign-in was cancelled in the browser".into(),
                        other => format!("Google sign-in failed: {other}"),
                    });
                    handled = true;
                } else if code.is_some() && got_state.as_deref() != Some(self.state.as_str()) {
                    method_err = Some("sign-in response did not match (stale tab?)".into());
                    handled = true;
                } else if let Some(code) = code {
                    let page = "<html><body style='font-family:sans-serif;padding:32px'>\
                        <h2>✅ Connected</h2><p>Google Calendar is linked. You can close \
                        this tab and return to the app.</p></body></html>";
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap_or_else(|_| tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap());
                    let _ = request.respond(
                        tiny_http::Response::from_string(page)
                            .with_status_code(200)
                            .with_header(header),
                    );
                    return Ok(code);
                }
            }
            if !handled {
                let _ = request.respond(
                    tiny_http::Response::from_string("not found").with_status_code(404),
                );
            }
            if let Some(err) = method_err {
                return Err(err);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Token exchange / refresh / revoke
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn tokens_from_response(resp: TokenResponse, previous_refresh: Option<&str>) -> Result<Tokens, String> {
    if let Some(err) = resp.error {
        return Err(format!(
            "{}{}",
            err,
            resp.error_description
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ));
    }
    let expires_in = resp.expires_in.unwrap_or(3600).max(60);
    Ok(Tokens {
        access_token: resp.access_token,
        // Google only returns refresh_token on first consent (or
        // prompt=consent); keep the previous one otherwise.
        refresh_token: resp.refresh_token.or_else(|| previous_refresh.map(str::to_string)),
        expires_at: now_millis() + (expires_in - 60) * 1000,
    })
}

pub fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    previous_refresh: Option<&str>,
) -> Result<Tokens, String> {
    let client = http_client()?;
    let resp: TokenResponse = client
        .post(TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ])
        .send()
        .map_err(|e| format!("token exchange request failed: {e}"))?
        .json()
        .map_err(|e| format!("token exchange response: {e}"))?;
    tokens_from_response(resp, previous_refresh)
}

pub fn refresh_tokens(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    previous_refresh: Option<&str>,
) -> Result<Tokens, String> {
    let client = http_client()?;
    let resp: TokenResponse = client
        .post(TOKEN_URL)
        .form(&[
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| format!("token refresh request failed: {e}"))?
        .json()
        .map_err(|e| format!("token refresh response: {e}"))?;
    tokens_from_response(resp, previous_refresh.or(Some(refresh_token)))
}

/// Best-effort revoke so the app stops appearing in the account's security
/// settings. Errors are reported but non-fatal for disconnect.
pub fn revoke(refresh_token: &str) -> Result<(), String> {
    let client = http_client()?;
    client
        .post(REVOKE_URL)
        .form(&[("token", refresh_token)])
        .send()
        .map_err(|e| format!("revoke request failed: {e}"))?;
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_is_wellformed() {
        let (v, c) = pkce_pair();
        // RFC 7636: verifier 43..128 chars from the unreserved set.
        assert!(v.len() >= 43 && v.len() <= 128);
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
        assert_eq!(c.len(), 43);
        // Challenge is deterministic for a given verifier.
        let digest = Sha256::digest(v.as_bytes());
        assert_eq!(B64URL.encode(digest), c);
    }

    #[test]
    fn auth_url_carries_required_params() {
        let url = build_auth_url(
            "my-client.apps.googleusercontent.com",
            "http://127.0.0.1:51234",
            "challenge123",
            "state456",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=my-client.apps.googleusercontent.com"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A51234"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=challenge123"));
        assert!(url.contains("state=state456"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.events"));
    }

    #[test]
    fn urldecode_roundtrip() {
        assert_eq!(urldecode("a%2Fb%20c+d"), "a/b c d");
        assert_eq!(urlencode("a/b c"), "a%2Fb%20c");
        assert_eq!(urldecode("plain"), "plain");
    }

    #[test]
    fn callback_server_binds_ephemeral_port() {
        let counter: ConnectGeneration = Arc::new(AtomicUsize::new(0));
        let server = ConnectServer::new(counter.clone()).unwrap();
        assert!(server.redirect_uri.starts_with("http://127.0.0.1:"));
        let port: u16 = server
            .redirect_uri
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        assert!(port > 0);
        // A second server gets a different port and bumps the generation.
        let _server2 = ConnectServer::new(counter.clone()).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn token_response_merge_keeps_previous_refresh() {
        let resp = TokenResponse {
            access_token: "at".into(),
            expires_in: Some(3600),
            refresh_token: None,
            error: None,
            error_description: None,
        };
        let t = tokens_from_response(resp, Some("old-refresh")).unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("old-refresh"));
        assert!(t.expires_at > now_millis());
    }
}
