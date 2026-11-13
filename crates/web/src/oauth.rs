//! OAuth for MCP clients, as the MCP spec's authorization asks: MergeLoop is the authorization
//! server, and signs the user in with GitHub. The access token is a MergeLoop session, as the
//! dashboard's is, so the user's GitHub token stays on the server.
//!
//! Clients register themselves (RFC 7591), and must redirect to a loopback address (RFC 8252):
//! the code never leaves the user's machine. But any program on it can start a sign-in, and
//! GitHub signs a user who signed in before back in without a question. So no code goes to a
//! client before the user says yes on the consent page, in the browser that signed in, and a yes
//! is never remembered: any program can register under any name.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Json};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use mergepatrol_github::setup::random_token;
use mergepatrol_store::Consent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::{self, McpClient};
use crate::blog::escape;

/// How long the user has to answer the consent page.
const CONSENT: i64 = 300;
/// The browser that signed in: the secret that binds a consent request to it.
const CONSENT_COOKIE: &str = "mp_consent";
/// The longest client name the page shows.
const NAME_MAX: usize = 100;

/// The MCP server's metadata (RFC 9728): who issues its tokens.
pub(crate) async fn resource(State(app): State<AppState>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", app.public_url),
        "authorization_servers": [&*app.public_url],
        "bearer_methods_supported": ["header"],
    }))
}

/// The authorization server's metadata (RFC 8414).
pub(crate) async fn server(State(app): State<AppState>) -> Json<Value> {
    let url = &app.public_url;
    Json(json!({
        "issuer": &**url,
        "authorization_endpoint": format!("{url}/oauth/authorize"),
        "token_endpoint": format!("{url}/oauth/token"),
        "registration_endpoint": format!("{url}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

#[derive(Deserialize)]
pub(crate) struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

/// Registers a client (RFC 7591). Nothing is kept: a client is only its loopback redirect,
/// which `authorize` checks again, and its name, which travels in its `client_id`.
pub(crate) async fn register(Json(r): Json<Registration>) -> Response {
    if r.redirect_uris.is_empty() || !r.redirect_uris.iter().all(|u| loopback(u)) {
        return oauth_error(
            "invalid_redirect_uri",
            "MergeLoop redirects only to a loopback address, such as http://127.0.0.1:<port>/callback.",
        );
    }
    if r.client_name.as_deref().is_some_and(|n| !valid_name(n)) {
        return oauth_error(
            "invalid_client_metadata",
            "client_name must be at most 100 characters, with no control characters.",
        );
    }
    let client_id = match &r.client_name {
        Some(name) => format!("{}.{}", random_token(16), BASE64URL.encode(name)),
        None => random_token(16),
    };
    let client = json!({
        "client_id": client_id,
        "client_name": r.client_name,
        "redirect_uris": r.redirect_uris,
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code"],
        "response_types": ["code"],
    });
    (StatusCode::CREATED, Json(client)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    /// The code the client's terminal shows: `mergeloop login`'s.
    user_code: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

pub(crate) async fn authorize(
    State(app): State<AppState>,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    // Without a redirect to trust, the error is the browser's to show.
    let Some(redirect_uri) = q.redirect_uri.filter(|u| loopback(u)) else {
        return (
            StatusCode::BAD_REQUEST,
            "This sign-in link has no loopback redirect_uri.",
        )
            .into_response();
    };
    let client = McpClient {
        redirect_uri,
        state: q.state,
        challenge: q.code_challenge.unwrap_or_default(),
        name: q.client_id.as_deref().and_then(name_of),
        user_code: q.user_code,
    };
    let valid = q.response_type.as_deref() == Some("code")
        && q.code_challenge_method.as_deref() == Some("S256")
        && auth::valid_challenge(&client.challenge)
        && client.user_code.as_deref().is_none_or(valid_user_code);
    if !valid {
        let mut url = format!(
            "{}{}error=invalid_request&error_description={}",
            client.redirect_uri,
            if client.redirect_uri.contains('?') {
                '&'
            } else {
                '?'
            },
            "response_type%20code%20with%20an%20S256%20code_challenge%2C%20and%20a%20user_code%20of%20letters%2C%20digits%20and%20dashes"
        );
        if let Some(state) = &client.state {
            url.push_str(&format!("&state={}", auth::percent_encode(state)));
        }
        return Redirect::to(&url).into_response();
    }
    auth::mcp_login(&app, &client).await
}

#[derive(Deserialize)]
pub(crate) struct TokenForm {
    grant_type: String,
    code: Option<String>,
    code_verifier: Option<String>,
}

/// Trades the one-time code and the PKCE verifier for the session token.
pub(crate) async fn token(State(app): State<AppState>, Form(f): Form<TokenForm>) -> Response {
    if f.grant_type != "authorization_code" {
        return oauth_error("unsupported_grant_type", "Only authorization_code.");
    }
    let (Some(code), Some(verifier)) = (f.code, f.code_verifier) else {
        return oauth_error("invalid_request", "code and code_verifier are required.");
    };
    match auth::redeem(&app, &code, &verifier).await {
        Ok(Some(token)) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "access_token": token, "token_type": "Bearer" })),
        )
            .into_response(),
        Ok(None) => oauth_error("invalid_grant", "This sign-in expired, or was used."),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "MCP token error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Asks the user, in the browser that signed in, whether `client` may have the session `token`
/// (of `login`). The cookie that binds the request to the browser, and the consent page's path.
pub(crate) async fn ask(
    app: &AppState,
    headers: &HeaderMap,
    client: &McpClient,
    token: &str,
    login: &str,
) -> anyhow::Result<(String, String)> {
    // A browser keeps its secret, so two sign-ins in one browser can both be answered.
    let browser = auth::read_cookie(headers, CONSENT_COOKIE)
        .filter(|b| b.len() == 32)
        .unwrap_or_else(|| random_token(24));
    let request = random_token(32);
    let request_hash = auth::hash(&request);
    let consent = Consent {
        browser_hash: auth::hash(&browser),
        token_hash: auth::hash(token),
        token: app.vault.seal(&consent_secret(&request_hash), token)?,
        client: serde_json::to_string(client)?,
        login: login.to_owned(),
        expires_at: auth::unix_now() + CONSENT,
    };
    app.store.create_consent(&request_hash, &consent).await?;
    let cookie = auth::cookie(
        app,
        CONSENT_COOKIE,
        &browser,
        Duration::from_secs(CONSENT as u64),
    );
    Ok((cookie, format!("/oauth/consent?request={request}")))
}

#[derive(Deserialize)]
pub(crate) struct ConsentQuery {
    request: Option<String>,
}

/// The consent page: who asks, what a yes gives it, and the code to compare.
pub(crate) async fn consent(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ConsentQuery>,
) -> Response {
    let request = q.request.unwrap_or_default();
    let found = async {
        let Some(browser) = browser_hash(&headers) else {
            return Ok(None);
        };
        let now = auth::unix_now();
        let Some(c) = app
            .store
            .consent(&auth::hash(&request), &browser, now)
            .await?
        else {
            return Ok(None);
        };
        anyhow::Ok(Some((
            serde_json::from_str::<McpClient>(&c.client)?,
            c.login,
        )))
    }
    .await;
    match found {
        Ok(Some((client, login))) => page(StatusCode::OK, &ask_page(&client, &login, &request)),
        Ok(None) => unanswerable(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Allow,
    Deny,
}

#[derive(Deserialize)]
pub(crate) struct Answer {
    request: String,
    decision: Decision,
}

/// The user's answer, from the consent page in the browser that signed in, once: the client gets
/// its one-time code, or OAuth's `access_denied`.
pub(crate) async fn answer(
    State(app): State<AppState>,
    headers: HeaderMap,
    Form(a): Form<Answer>,
) -> Response {
    let Some(browser) = browser_hash(&headers) else {
        return unanswerable();
    };
    let request_hash = auth::hash(&a.request);
    let result = async {
        let Some(consent) = app
            .store
            .take_consent(&request_hash, &browser, auth::unix_now())
            .await?
        else {
            return Ok(None);
        };
        let client: McpClient = serde_json::from_str(&consent.client)?;
        let code = match a.decision {
            Decision::Allow => {
                let token = app
                    .vault
                    .open(&consent_secret(&request_hash), &consent.token)?;
                let token = secrecy::ExposeSecret::expose_secret(&token);
                Ok(auth::login_code(&app, token, &client.challenge).await?)
            }
            Decision::Deny => {
                app.store.delete_session(&consent.token_hash).await?;
                Err(anyhow::anyhow!("You denied the sign-in."))
            }
        };
        anyhow::Ok(Some(auth::mcp_redirect(&client, code)))
    }
    .await;
    match result {
        Ok(Some(to)) => Redirect::to(&to).into_response(),
        Ok(None) => unanswerable(),
        Err(e) => internal(e),
    }
}

/// The hash of the browser's consent secret, if it has one.
fn browser_hash(headers: &HeaderMap) -> Option<String> {
    auth::read_cookie(headers, CONSENT_COOKIE)
        .filter(|b| !b.is_empty())
        .map(|b| auth::hash(&b))
}

/// Where a request's session token is sealed: it opens for that request only.
fn consent_secret(request_hash: &str) -> String {
    format!("consent/{request_hash}")
}

fn unanswerable() -> Response {
    page(
        StatusCode::FORBIDDEN,
        "<h1>This sign-in cannot be answered here</h1>\
         <p>It expired, was answered already, or started in another browser. Start the sign-in \
         again from the program that asked.</p>",
    )
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %format!("{e:#}"), "consent error");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// The question, in plain words. Every client redirects to a loopback address, so every client
/// is a program on this computer.
fn ask_page(client: &McpClient, login: &str, request: &str) -> String {
    let name = match &client.name {
        Some(name) => format!(
            "It calls itself <strong>{}</strong>, a name any program can give itself.",
            escape(name)
        ),
        None => "It gave no name.".to_owned(),
    };
    let code = match &client.user_code {
        Some(code) => format!(
            "<p class=\"code\">Code: <strong>{}</strong></p>\
             <p>This code must match the one your terminal shows. If it does not, or you did not \
             just start this sign-in, deny it.</p>",
            escape(code)
        ),
        None => "<p><strong>No code was given.</strong> <code>mergeloop login</code> always \
                 shows one, so this is not it. Allow only if you just started this sign-in \
                 yourself, from the program named above.</p>"
            .to_owned(),
    };
    format!(
        "<h1>Allow this sign-in?</h1>\
         <p>A program on this computer asks to sign in to MergeLoop as you, \
         <strong>{login}</strong>. {name}</p>\
         <p>If you allow it, it acts as you in MergeLoop: it can do anything you can do there, \
         in every workspace you can reach, until it goes 30 days without using the sign-in.</p>\
         {code}\
         <form method=\"post\" action=\"/oauth/consent\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <button name=\"decision\" value=\"deny\">Deny</button> \
         <button name=\"decision\" value=\"allow\">Allow</button>\
         </form>",
        login = escape(login),
        request = escape(request),
    )
}

/// A page of the sign-in: never framed, so no other page can trick a click onto it; its URL
/// carries the request, so it goes in no `Referer`.
fn page(status: StatusCode, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Sign in · MergeLoop</title><style>{STYLE}</style></head>\
         <body><main>{body}</main></body></html>"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

const STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;margin:0;padding:16px;\
    background:#fff;color:#111}main{max-width:32rem;margin:10vh auto}\
    .code strong{font:600 1.5rem ui-monospace,monospace;letter-spacing:.1em}\
    button{font:inherit;padding:.5rem 1.25rem;margin-right:.5rem}\
    @media (prefers-color-scheme:dark){body{background:#111;color:#eee}}";

/// A name the page can show.
fn valid_name(name: &str) -> bool {
    name.chars().count() <= NAME_MAX && !name.chars().any(char::is_control)
}

/// The name a `client_id` from `register` carries. `None` for one without, or not from here.
fn name_of(client_id: &str) -> Option<String> {
    let (_, name) = client_id.split_once('.')?;
    let name = String::from_utf8(BASE64URL.decode(name).ok()?).ok()?;
    valid_name(&name).then_some(name)
}

/// A code a terminal can show: letters, digits and dashes.
fn valid_user_code(code: &str) -> bool {
    (1..=16).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn oauth_error(error: &str, description: &str) -> Response {
    let body = json!({ "error": error, "error_description": description });
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// `http` to this machine: what a native app listens on. No fragment, which would hide the
/// code from it.
pub(crate) fn loopback(uri: &str) -> bool {
    if uri.contains('#') {
        return false;
    }
    let Ok(uri) = uri.parse::<Uri>() else {
        return false;
    };
    uri.scheme_str() == Some("http")
        && matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_only_to_this_machine() {
        assert!(loopback("http://127.0.0.1:33418/callback"));
        assert!(loopback("http://localhost:6274/oauth/callback?x=1"));
        assert!(loopback("http://[::1]:8080/cb"));
        assert!(!loopback("https://127.0.0.1/cb"));
        assert!(!loopback("http://127.0.0.1.evil.com/cb"));
        assert!(!loopback("http://localhost@evil.com/cb"));
        assert!(!loopback("https://claude.ai/api/mcp/auth_callback"));
        assert!(!loopback("/callback"));
        assert!(!loopback("http://127.0.0.1:9/cb#x"));
    }
}
