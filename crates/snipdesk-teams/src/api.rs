//! HTTP client for the snipdesk-server API. Mirrors the wire shapes
//! defined in `crates/snipdesk-server/src/handlers/` — we redefine them
//! here rather than depending on the server crate to keep the workspace
//! dependency graph one-way (client → server contract, no shared
//! types crate, which would tangle Lite builds with server symbols).
//!
//! All calls are synchronous (`ureq`); the sync engine runs them on a
//! dedicated background thread so the UI never blocks on the network.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors the API client can surface. The sync engine and IPC layer
/// pattern-match on variants — `VersionConflict` in particular is the
/// happy-path branch where two clients edited the same snippet, not an
/// error to bubble up to the user.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(String),

    /// Stale `expected_version` on an update. The sync engine handles
    /// this by re-pulling and applying last-write-wins.
    #[error("version conflict")]
    VersionConflict,

    /// HTTP 401 from any endpoint. Sync engine treats this as "session
    /// expired; sign in again."
    #[error("unauthorized")]
    Unauthorized,

    /// The server returned a 4xx/5xx we didn't classify specifically.
    /// `code` is the machine string ("invalid_email", "weak_password",
    /// ...); `message` is the human detail.
    #[error("{code}: {message}")]
    Server {
        status: u16,
        code: String,
        message: String,
    },

    /// Server returned something we couldn't parse as the expected
    /// shape. Almost always a version skew between client and server.
    #[error("bad response: {0}")]
    Decode(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

// ---- Wire-shape mirrors ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDto {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetPayload {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WriteResponse {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnippetView {
    pub id: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub is_deleted: bool,
    pub payload: Option<SnippetPayload>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncResponse {
    pub snippets: Vec<SnippetView>,
    pub high_water_mark: i64,
}

// ---- Helpers ----

/// Server returns errors as `{ "error": "code", "message": "..." }`.
/// We decode that shape so the caller can branch on `code`.
#[derive(Debug, Deserialize)]
struct ServerError {
    error: String,
    #[serde(default)]
    message: String,
}

fn url(base: &str, path: &str) -> String {
    // Trim a trailing slash so `https://snip.example.com/` + `/api/x`
    // doesn't produce `//api/x`. Servers tolerate both but ugly logs.
    let base = base.trim_end_matches('/');
    format!("{base}{path}")
}

fn handle_response<T: serde::de::DeserializeOwned>(
    res: Result<ureq::Response, ureq::Error>,
) -> ApiResult<T> {
    match res {
        Ok(resp) => resp
            .into_json::<T>()
            .map_err(|e| ApiError::Decode(e.to_string())),
        Err(ureq::Error::Status(status, resp)) => {
            // Try to decode the structured error body; fall back to a
            // generic "server error" if the server didn't follow shape.
            let parsed = resp.into_json::<ServerError>().ok();
            let (code, message) = match parsed {
                Some(p) => (p.error, p.message),
                None => ("server_error".into(), format!("HTTP {status}")),
            };
            Err(match (status, code.as_str()) {
                (401, _) => ApiError::Unauthorized,
                (409, "version_conflict") => ApiError::VersionConflict,
                _ => ApiError::Server {
                    status,
                    code,
                    message,
                },
            })
        }
        Err(ureq::Error::Transport(t)) => Err(ApiError::Network(t.to_string())),
    }
}

fn handle_unit(res: Result<ureq::Response, ureq::Error>) -> ApiResult<()> {
    match res {
        Ok(_) => Ok(()),
        Err(e) => {
            // Reuse the typed error path; 204 doesn't decode but we
            // wouldn't reach here on success anyway.
            let dummy: ApiResult<serde_json::Value> = handle_response(Err(e));
            dummy.map(|_| ())
        }
    }
}

// ---- Endpoints ----

#[derive(Debug, Serialize)]
struct SignupBody<'a> {
    email: &'a str,
    password: &'a str,
    display_name: &'a str,
}

pub fn signup(
    server_url: &str,
    email: &str,
    password: &str,
    display_name: &str,
) -> ApiResult<AuthResponse> {
    let res = ureq::post(&url(server_url, "/api/auth/signup"))
        .set("content-type", "application/json")
        .send_json(SignupBody {
            email,
            password,
            display_name,
        });
    handle_response(res)
}

#[derive(Debug, Serialize)]
struct LoginBody<'a> {
    email: &'a str,
    password: &'a str,
}

pub fn login(server_url: &str, email: &str, password: &str) -> ApiResult<AuthResponse> {
    let res = ureq::post(&url(server_url, "/api/auth/login"))
        .set("content-type", "application/json")
        .send_json(LoginBody { email, password });
    handle_response(res)
}

#[derive(Debug, Deserialize)]
pub struct MeResponse {
    pub user: UserDto,
}

pub fn me(server_url: &str, token: &str) -> ApiResult<MeResponse> {
    let res = ureq::get(&url(server_url, "/api/me"))
        .set("authorization", &format!("Bearer {token}"))
        .call();
    handle_response(res)
}

pub fn list_snippets(server_url: &str, token: &str, since: i64) -> ApiResult<SyncResponse> {
    let res = ureq::get(&url(server_url, "/api/snippets"))
        .set("authorization", &format!("Bearer {token}"))
        .query("since", &since.to_string())
        .call();
    handle_response(res)
}

#[derive(Debug, Serialize)]
pub struct CreateBody<'a> {
    pub id: &'a str,
    #[serde(flatten)]
    pub payload: &'a SnippetPayload,
}

pub fn create_snippet(
    server_url: &str,
    token: &str,
    body: &CreateBody<'_>,
) -> ApiResult<WriteResponse> {
    let res = ureq::post(&url(server_url, "/api/snippets"))
        .set("authorization", &format!("Bearer {token}"))
        .set("content-type", "application/json")
        .send_json(body);
    handle_response(res)
}

#[derive(Debug, Serialize)]
pub struct UpdateBody<'a> {
    pub expected_version: i64,
    #[serde(flatten)]
    pub payload: &'a SnippetPayload,
}

pub fn update_snippet(
    server_url: &str,
    token: &str,
    id: &str,
    body: &UpdateBody<'_>,
) -> ApiResult<WriteResponse> {
    let path = format!("/api/snippets/{id}");
    let res = ureq::put(&url(server_url, &path))
        .set("authorization", &format!("Bearer {token}"))
        .set("content-type", "application/json")
        .send_json(body);
    handle_response(res)
}

pub fn delete_snippet(server_url: &str, token: &str, id: &str) -> ApiResult<()> {
    let path = format!("/api/snippets/{id}");
    let res = ureq::delete(&url(server_url, &path))
        .set("authorization", &format!("Bearer {token}"))
        .call();
    handle_unit(res)
}
