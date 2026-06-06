//! Dashboard page handlers.
//!
//! Style:
//!   - One handler per URL/method, mounted from `dashboard::routes()`.
//!   - htmx triggers POST/PUT/DELETE for in-place updates; the response
//!     is the new HTML fragment for the affected row/card, NOT a full
//!     page reload. Full-page handlers (`*_page`) return the layout +
//!     content; htmx-only handlers (`*_row`, `*_card`) return just the
//!     fragment.
//!   - HTML is hand-rolled via `include_str!` + `{{KEY}}` substitution.
//!     Anything user-controlled goes through `escape_html()` before
//!     ending up inside a template - never trust input.
//!
//! What this doesn't do (deferred to later phases):
//!   - Edit user display name / email (the admin tool exists to manage
//!     access, not to maintain user records - users edit their own).
//!   - Filter / paginate the users list (the table is small enough for
//!     this not to matter at v1 scale; SQLite + ORDER BY is fine).
//!   - Per-user snippet activity timeline (we surface counts, not
//!     content - the dashboard never reveals personal snippet bodies).

use std::collections::HashMap;

use axum::extract::{Form, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use chrono::{TimeZone, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{hash_password, issue_token, verify_password_constant_time};
use crate::dashboard::session::{
    build_cookie, clear_cookie, fetch_nav_user, safe_next, DashboardAdmin, DashboardSession,
};
use crate::http::AppState;

// ---- Templates baked in at compile time. ----

const LAYOUT: &str = include_str!("templates/layout.html");
const LOGIN: &str = include_str!("templates/login.html");

// ---- Tiny templating helper ----

/// Replace every `{{KEY}}` occurrence in `tpl` with its value. Order is
/// deterministic (sorted keys) so a value containing `{{OTHER_KEY}}`
/// from a previous substitution can't accidentally trigger a second
/// round of substitution - keys are processed once each and the
/// remaining `{{...}}` placeholders are dropped at the end so unused
/// slots don't leak.
fn render(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut out = tpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    // Strip any unfilled placeholders so they don't visibly leak into
    // the rendered page (typo guard for the developer; user never sees).
    let mut cleaned = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"{{") {
            if let Some(end) = out[i..].find("}}") {
                i += end + 2;
                continue;
            }
        }
        cleaned.push(bytes[i] as char);
        i += 1;
    }
    cleaned
}

/// HTML-escape user-controlled text. Cheap and complete for HTML body
/// context; do NOT trust this in attribute context without quoting (we
/// always quote, so this set is sufficient).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a full page wrapped in the layout. The two `*_ACTIVE` slots
/// gate nav-link highlighting; pass "active" for the current page,
/// empty string for the others.
async fn render_page(
    state: &AppState,
    session: &DashboardSession,
    title: &str,
    users_active: bool,
    library_active: bool,
    content: &str,
) -> Html<String> {
    let (display, role) = fetch_nav_user(state, &session.claims).await;
    Html(render(
        LAYOUT,
        &[
            ("TITLE", title),
            ("USERS_ACTIVE", if users_active { "active" } else { "" }),
            ("LIBRARY_ACTIVE", if library_active { "active" } else { "" }),
            ("NAV_USER", &escape_html(&display)),
            ("NAV_ROLE", &escape_html(&role)),
            ("CONTENT", content),
        ],
    ))
}

// ---- / (index - login or redirect) ----

#[derive(Debug, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    expired: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    redirect_to: Option<String>,
}

pub async fn index(
    State(_state): State<AppState>,
    Query(q): Query<IndexQuery>,
    jar: CookieJar,
) -> Response {
    // If the cookie is present and decodes to an admin claim, skip the
    // login form and send them in. (Members logged into the cookie get
    // bounced when they hit /dashboard/users.) We don't validate the
    // role here - that's the admin extractor's job - so a member with
    // a fresh session gets to see the bounce page once, not the login
    // form with a confusing "still signed in" experience.
    let signed_in = jar.get(crate::dashboard::session::COOKIE_NAME).is_some();
    if signed_in {
        return Redirect::to("/dashboard/users").into_response();
    }
    let banner = if q.expired.is_some() {
        "<div class=\"banner info\">Your session expired. Sign in again.</div>"
    } else if q.error.as_deref() == Some("invalid") {
        "<div class=\"banner error\">Invalid email or password.</div>"
    } else if q.error.as_deref() == Some("disabled") {
        "<div class=\"banner error\">Your account is disabled. Contact your administrator.</div>"
    } else {
        ""
    };
    Html(render(
        LOGIN,
        &[
            ("BANNER", banner),
            (
                "REDIRECT_TO",
                &escape_html(&safe_next(q.redirect_to.as_deref())),
            ),
        ],
    ))
    .into_response()
}

// ---- /dashboard/login (POST) ----

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub redirect_to: Option<String>,
}

pub async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(body): Form<LoginForm>,
) -> Response {
    let email = body.email.trim().to_lowercase();

    let row: Option<LoginRow> =
        sqlx::query_as("SELECT id, role, password_hash, is_disabled FROM users WHERE email = ?")
            .bind(&email)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);

    let stored = row.as_ref().and_then(|r| r.password_hash.as_deref());
    let ok = verify_password_constant_time(&body.password, stored);

    let row = match (ok, row) {
        (true, Some(r)) => r,
        _ => return Redirect::to("/?error=invalid").into_response(),
    };
    if row.is_disabled != 0 {
        return Redirect::to("/?error=disabled").into_response();
    }

    let token = match issue_token(&row.id, &row.role, &state.jwt_secret) {
        Ok(t) => t,
        Err(_) => return Redirect::to("/?error=invalid").into_response(),
    };

    // Update last_seen_at on dashboard sign-in too - admins are users
    // and should show up alive on the users page.
    let now = Utc::now().timestamp();
    let _ = sqlx::query("UPDATE users SET last_seen_at = ? WHERE id = ?")
        .bind(now)
        .bind(&row.id)
        .execute(&state.pool)
        .await;

    let next = safe_next(body.redirect_to.as_deref());
    let jar = jar.add(build_cookie(token));
    (jar, Redirect::to(&next)).into_response()
}

#[derive(sqlx::FromRow)]
struct LoginRow {
    id: String,
    role: String,
    password_hash: Option<String>,
    is_disabled: i64,
}

// ---- /dashboard/logout (POST) ----

pub async fn logout(jar: CookieJar) -> Response {
    let jar = jar.add(clear_cookie());
    (jar, Redirect::to("/")).into_response()
}

// ---- /dashboard/users (GET) ----

pub async fn users_page(State(state): State<AppState>, admin: DashboardAdmin) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };
    let rows = match load_users(&state).await {
        Ok(r) => r,
        Err(_) => {
            return render_page(
                &state,
                &session,
                "Users",
                true,
                false,
                "<div class=\"banner error\">Failed to load users.</div>",
            )
            .await
            .into_response();
        }
    };
    let my_id = admin.user_id().to_string();
    let mut body = String::new();
    body.push_str("<h1>Users</h1>");
    body.push_str(&new_user_form());
    // The tbody polls itself every 5s so another admin's changes (or
    // CLI / console mutations) appear without a manual refresh. The
    // poll fetches /dashboard/users/rows which returns only the rows
    // (no table chrome) and swaps them into this tbody's innerHTML.
    // Inline row actions (promote/demote/delete) still work because
    // htmx re-binds attributes after every swap.
    body.push_str(
        "<table class=\"data\"><thead><tr>\
         <th>Name</th><th>Email</th><th>Role</th><th>Snippets</th><th>Last seen</th><th>Status</th><th class=\"col-actions\"></th>\
         </tr></thead><tbody id=\"users-tbody\" \
            hx-get=\"/dashboard/users/rows\" \
            hx-trigger=\"every 5s\" \
            hx-swap=\"innerHTML\">",
    );
    for u in &rows {
        body.push_str(&render_user_row(u, &my_id));
    }
    body.push_str("</tbody></table>");

    render_page(&state, &session, "Users", true, false, &body)
        .await
        .into_response()
}

/// Fragment endpoint: just the `<tr>` rows for the users tbody. Used by
/// the polling tick on `/dashboard/users` so updates from other admins,
/// CLI / console commands, etc. surface without a manual refresh.
pub async fn users_rows(State(state): State<AppState>, admin: DashboardAdmin) -> Response {
    let rows = match load_users(&state).await {
        Ok(r) => r,
        Err(_) => {
            return (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                "<tr><td colspan=\"7\" class=\"banner error\">Failed to load users.</td></tr>",
            )
                .into_response();
        }
    };
    let my_id = admin.user_id().to_string();
    let mut body = String::new();
    for u in &rows {
        body.push_str(&render_user_row(u, &my_id));
    }
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Inline "Create user" form. Lives at the top of the users page so an
/// admin can add a teammate without poking around. Submitted via htmx
/// so a successful add prepends the row inline.
fn new_user_form() -> String {
    String::from(
        "<details class=\"lib-form\" style=\"margin-bottom:16px;\">\
           <summary>Add user</summary>\
           <form class=\"stack\" \
                 hx-post=\"/dashboard/users\" \
                 hx-target=\"#users-tbody\" \
                 hx-swap=\"afterbegin\" \
                 hx-on::after-request=\"if(event.detail.successful) this.reset()\" \
                 style=\"margin-top:10px\">\
             <div class=\"row\">\
               <label>Display name<input type=\"text\" name=\"display_name\" required /></label>\
               <label>Email<input type=\"email\" name=\"email\" required /></label>\
             </div>\
             <div class=\"row\">\
               <label>Password (min 10 chars)<input type=\"text\" name=\"password\" required minlength=\"10\" /></label>\
               <label>Role\
                 <select name=\"role\"><option value=\"member\">member</option><option value=\"admin\">admin</option></select>\
               </label>\
             </div>\
             <div class=\"actions\"><button class=\"primary\" type=\"submit\">Create user</button></div>\
           </form>\
         </details>",
    )
}

async fn load_users(state: &AppState) -> Result<Vec<crate::handlers::admin::AdminUserView>, ()> {
    // Reuse the admin handler's SELECT exactly, since the dashboard
    // page IS the admin handler in HTML form. Keeping a single source
    // of truth means a future column add lands in both views.
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.created_at, u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         GROUP BY u.id \
         ORDER BY u.created_at ASC",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| ())?;

    Ok(rows
        .into_iter()
        .map(|r| crate::handlers::admin::AdminUserView {
            id: r.id,
            email: r.email,
            display_name: r.display_name,
            role: r.role,
            is_disabled: r.is_disabled != 0,
            created_at: r.created_at,
            last_seen_at: r.last_seen_at,
            snippet_count: r.snippet_count,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: String,
    email: String,
    display_name: String,
    role: String,
    is_disabled: i64,
    created_at: i64,
    last_seen_at: Option<i64>,
    snippet_count: i64,
}

/// Render one user row. Pure function so it can be returned alone (for
/// htmx out-of-band swaps) or composed into the full table. `me_id`
/// suppresses self-targeted action buttons.
fn render_user_row(u: &crate::handlers::admin::AdminUserView, me_id: &str) -> String {
    let last_seen = match u.last_seen_at {
        None => "never".to_string(),
        Some(ts) => format_relative(ts),
    };
    let role_pill = format!(
        "<span class=\"pill role-{role}\">{role}</span>",
        role = escape_html(&u.role)
    );
    let status_pill = if u.is_disabled {
        "<span class=\"pill disabled\">disabled</span>".to_string()
    } else {
        "<span class=\"muted\">active</span>".to_string()
    };
    let actions = if u.id == me_id {
        // Self-row: no buttons. Anything we'd offer would be a
        // self-lockout risk, and the server-side gates already block
        // them - better to hide than to show a button that always 400s.
        "<span class=\"muted\">- you -</span>".to_string()
    } else {
        let toggle_role = if u.role == "admin" {
            ("member", "Demote")
        } else {
            ("admin", "Promote")
        };
        let toggle_disabled = if u.is_disabled {
            (false, "Enable")
        } else {
            (true, "Disable")
        };
        format!(
            "<button class=\"btn\" \
                hx-put=\"/dashboard/users/{id}\" \
                hx-vals='{{\"role\":\"{role}\"}}' \
                hx-target=\"closest tr\" hx-swap=\"outerHTML\">{label}</button> \
             <button class=\"btn\" \
                hx-put=\"/dashboard/users/{id}\" \
                hx-vals='{{\"is_disabled\":{flag}}}' \
                hx-target=\"closest tr\" hx-swap=\"outerHTML\">{dlabel}</button> \
             <button class=\"btn danger\" \
                hx-delete=\"/dashboard/users/{id}\" \
                hx-confirm=\"Delete {name}? Their personal snippets will be removed from the server.\" \
                hx-target=\"closest tr\" hx-swap=\"outerHTML\">Delete</button>",
            id = escape_html(&u.id),
            role = toggle_role.0,
            label = toggle_role.1,
            flag = toggle_disabled.0,
            dlabel = toggle_disabled.1,
            name = escape_html(&u.display_name),
        )
    };
    let row_class = if u.is_disabled {
        " class=\"disabled\""
    } else {
        ""
    };
    format!(
        "<tr{row_class} id=\"user-{id_attr}\">\
         <td>{name}</td>\
         <td class=\"mono muted\">{email}</td>\
         <td>{role_pill}</td>\
         <td>{count}</td>\
         <td class=\"muted\">{last_seen}</td>\
         <td>{status_pill}</td>\
         <td class=\"col-actions\">{actions}</td>\
         </tr>",
        id_attr = escape_html(&u.id),
        name = escape_html(&u.display_name),
        email = escape_html(&u.email),
        count = u.snippet_count,
    )
}

/// "5 min ago", "yesterday", "2 days ago", "Mar 14". Cheap and good
/// enough for the dashboard - no humantime dep.
fn format_relative(ts: i64) -> String {
    let now = Utc::now().timestamp();
    let delta = now - ts;
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3600 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h ago", delta / 3600);
    }
    if delta < 7 * 86_400 {
        return format!("{}d ago", delta / 86_400);
    }
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_else(|| "-".to_string())
}

// ---- /dashboard/users (POST) - create user ----

#[derive(Debug, Deserialize)]
pub struct CreateUserForm {
    pub email: String,
    pub display_name: String,
    pub password: String,
    pub role: String,
}

pub async fn user_create_row(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Form(body): Form<CreateUserForm>,
) -> Response {
    let email = body.email.trim().to_lowercase();
    let display_name = body.display_name.trim().to_string();
    let role = if body.role == "admin" {
        "admin"
    } else {
        "member"
    };

    if !email.contains('@') || display_name.is_empty() || body.password.len() < 10 {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<tr><td colspan=\"7\" class=\"banner error\">Invalid input - check email, name, and 10-char password.</td></tr>",
        ).into_response();
    }

    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = ?")
        .bind(&email)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    if existing.is_some() {
        return (
            StatusCode::CONFLICT,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<tr><td colspan=\"7\" class=\"banner error\">An account with that email already exists.</td></tr>",
        ).into_response();
    }

    let password_hash = match hash_password(&body.password) {
        Ok(h) => h,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    if sqlx::query(
        "INSERT INTO users (id, email, display_name, role, is_disabled, created_at, password_hash) \
         VALUES (?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(&email)
    .bind(&display_name)
    .bind(role)
    .bind(now)
    .bind(&password_hash)
    .execute(&state.pool)
    .await
    .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let view = crate::handlers::admin::AdminUserView {
        id,
        email,
        display_name,
        role: role.to_string(),
        is_disabled: false,
        created_at: now,
        last_seen_at: None,
        snippet_count: 0,
    };
    (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_user_row(&view, "irrelevant"),
    )
        .into_response()
}

// ---- /dashboard/users/:id (PUT) - disable/enable + role ----
//
// Reuses the JSON admin handler under the hood: we already validated
// rules there (self-protection, last-admin, role values). Calling the
// handler directly means the dashboard and the API can't diverge.

#[derive(Debug, Deserialize)]
pub struct UserUpdateForm {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    is_disabled: Option<UpdateFlag>,
}

/// htmx encodes booleans as the JS literal - `true` or `false` -
/// inside hx-vals JSON, but axum's Form codec posts them as strings.
/// `UpdateFlag` accepts either shape.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum UpdateFlag {
    Bool(bool),
    Str(String),
}

impl UpdateFlag {
    fn as_bool(&self) -> Option<bool> {
        match self {
            UpdateFlag::Bool(b) => Some(*b),
            UpdateFlag::Str(s) => match s.trim() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
        }
    }
}

pub async fn user_update_row(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
    Form(body): Form<UserUpdateForm>,
) -> Response {
    let auth = crate::auth::AuthUser(admin.claims.clone());
    let res = crate::handlers::admin::update_user(
        State(state.clone()),
        auth,
        Path(id.clone()),
        Json(crate::handlers::admin::UpdateUserBody {
            role: body.role,
            is_disabled: body.is_disabled.as_ref().and_then(UpdateFlag::as_bool),
        }),
    )
    .await;
    let me_id = admin.user_id().to_string();
    match res {
        Ok(Json(view)) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_user_row(&view, &me_id),
        )
            .into_response(),
        Err(err) => (
            err.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                "<tr><td colspan=\"7\" class=\"banner error\">{}</td></tr>",
                escape_html(&err.message)
            ),
        )
            .into_response(),
    }
}

pub async fn user_delete_row(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
) -> Response {
    let auth = crate::auth::AuthUser(admin.claims.clone());
    match crate::handlers::admin::delete_user(State(state.clone()), auth, Path(id)).await {
        Ok(_) => (
            // Empty body + 200 → htmx swaps the target with nothing,
            // i.e. the row disappears. Returning 204 would skip the
            // swap entirely.
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "",
        )
            .into_response(),
        Err(err) => (
            err.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                "<tr><td colspan=\"7\" class=\"banner error\">{}</td></tr>",
                escape_html(&err.message)
            ),
        )
            .into_response(),
    }
}

// ---- /dashboard/library ----

pub async fn library_page(State(state): State<AppState>, admin: DashboardAdmin) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };
    let rows = load_library(&state).await.unwrap_or_default();
    let mut body = String::new();
    body.push_str("<h1>Shared library</h1>");
    body.push_str("<p class=\"muted\">Snippets here appear in every signed-in member's Team Library sidebar. They're plaintext at rest - don't put secrets in.</p>");
    body.push_str(&library_create_form());
    // Polls itself every 5s so another admin's adds / edits / deletes
    // surface without a manual refresh.
    body.push_str(
        "<div class=\"library-list\" id=\"library-list\" \
              hx-get=\"/dashboard/library/cards\" \
              hx-trigger=\"every 5s\" \
              hx-swap=\"innerHTML\">",
    );
    body.push_str(&render_library_cards_inner(&rows));
    body.push_str("</div>");

    render_page(&state, &session, "Library", false, true, &body)
        .await
        .into_response()
}

/// Fragment endpoint: just the library cards (no outer container).
/// Hit by the polling tick on `/dashboard/library`.
pub async fn library_cards(State(state): State<AppState>, _admin: DashboardAdmin) -> Response {
    let rows = load_library(&state).await.unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_library_cards_inner(&rows),
    )
        .into_response()
}

/// Shared body of the cards container; same output whether we're
/// rendering the initial page or a polling refresh.
fn render_library_cards_inner(rows: &[LibraryRow]) -> String {
    if rows.is_empty() {
        return String::from("<p class=\"muted\">No library snippets yet. Add one above.</p>");
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&render_library_card(r));
    }
    out
}

fn library_create_form() -> String {
    String::from(
        "<form class=\"lib-form stack\" \
              hx-post=\"/dashboard/library\" \
              hx-target=\"#library-list\" \
              hx-swap=\"afterbegin\" \
              hx-on::after-request=\"if(event.detail.successful) this.reset()\">\
           <div class=\"row\">\
             <label>Title<input type=\"text\" name=\"title\" required /></label>\
             <label>Folder<input type=\"text\" name=\"folder_path\" placeholder=\"e.g. Replies/Billing\" /></label>\
           </div>\
           <label>Body<textarea name=\"body\" required></textarea></label>\
           <label>Tags (comma-separated)<input type=\"text\" name=\"tags\" placeholder=\"billing, refund\" /></label>\
           <div class=\"actions\"><button class=\"primary\" type=\"submit\">Add to library</button></div>\
         </form>",
    )
}

fn render_library_card(r: &LibraryRow) -> String {
    let tags_html = if r.tags.trim().trim_matches(',').is_empty() {
        String::new()
    } else {
        let pills: Vec<String> = r
            .tags
            .split(',')
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!("<span class=\"pill\">{}</span>", escape_html(t.trim())))
            .collect();
        format!(" {}", pills.join(" "))
    };
    let folder = match &r.folder_path {
        Some(f) if !f.is_empty() => format!(" · <span class=\"muted\">{}</span>", escape_html(f)),
        _ => String::new(),
    };
    format!(
        "<div class=\"library-card\" id=\"lib-{id_attr}\">\
           <div class=\"card-head\">\
             <span class=\"title\">{title}</span>{folder}{tags}\
             <span class=\"meta\">v{ver} · updated {when}</span>\
           </div>\
           <pre class=\"body\">{body}</pre>\
           <div class=\"card-actions\">\
             <button class=\"btn\" \
                hx-get=\"/dashboard/library/{id_attr}/edit\" \
                hx-target=\"#lib-{id_attr}\" hx-swap=\"outerHTML\" disabled \
                title=\"Inline edit lands in phase 8\">Edit</button>\
             <button class=\"btn danger\" \
                hx-delete=\"/dashboard/library/{id_attr}\" \
                hx-confirm=\"Delete library snippet '{title_attr}'?\" \
                hx-target=\"closest .library-card\" hx-swap=\"outerHTML\">Delete</button>\
           </div>\
         </div>",
        id_attr = escape_html(&r.id),
        title = escape_html(&r.title),
        title_attr = escape_html(&r.title),
        body = escape_html(&r.body),
        ver = r.version,
        when = format_relative(r.updated_at),
        tags = tags_html,
        folder = folder,
    )
}

async fn load_library(state: &AppState) -> Result<Vec<LibraryRow>, ()> {
    sqlx::query_as::<_, LibraryRow>(
        "SELECT id, title, body, tags, folder_path, version, updated_at \
         FROM library_snippets \
         WHERE is_deleted = 0 \
         ORDER BY updated_at DESC",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| ())
}

#[derive(sqlx::FromRow)]
struct LibraryRow {
    id: String,
    title: String,
    body: String,
    tags: String,
    folder_path: Option<String>,
    version: i64,
    updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct LibraryCreateForm {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub folder_path: String,
}

pub async fn library_create(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Form(body): Form<LibraryCreateForm>,
) -> Response {
    let title = body.title.trim();
    if title.is_empty() || body.body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "<div class=\"banner error\">Title and body are required.</div>",
        )
            .into_response();
    }
    let tags: Vec<String> = body
        .tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let folder = body.folder_path.trim();
    let folder_opt = if folder.is_empty() {
        None
    } else {
        Some(folder.to_string())
    };
    let id = Uuid::new_v4().to_string();

    let auth = crate::auth::AuthUser(admin.claims.clone());
    let res = crate::handlers::library::create(
        State(state.clone()),
        auth,
        Json(crate::handlers::library::CreateBody {
            id: id.clone(),
            payload: crate::handlers::library::LibraryPayload {
                title: title.to_string(),
                body: body.body.clone(),
                tags: tags.clone(),
                folder_path: folder_opt.clone(),
            },
        }),
    )
    .await;
    match res {
        Ok((_, Json(write))) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_library_card(&LibraryRow {
                id: write.id,
                title: title.to_string(),
                body: body.body,
                // encode_tags shape matches what the server stores so
                // the rendered card looks identical to a fresh fetch.
                tags: encode_tags_inline(&tags),
                folder_path: folder_opt,
                version: write.version,
                updated_at: write.updated_at,
            }),
        )
            .into_response(),
        Err(err) => (
            err.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                "<div class=\"banner error\">{}</div>",
                escape_html(&err.message)
            ),
        )
            .into_response(),
    }
}

/// Mirror of the server-side encode_tags so the rendered card looks
/// identical to a fresh-fetched row. Cheap; this is only called on the
/// success path of a create.
fn encode_tags_inline(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut s = String::from(",");
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() {
            s.push_str(&t);
            s.push(',');
        }
    }
    s
}

pub async fn library_update(
    State(_state): State<AppState>,
    _admin: DashboardAdmin,
    Path(_id): Path<String>,
    Form(_form): Form<HashMap<String, String>>,
) -> Response {
    // Stub: phase 8 will add inline edit. Today an admin deletes and
    // re-creates if they want to change a library snippet, which is
    // imperfect but acceptable for the small libraries this version
    // ships against.
    (
        StatusCode::NOT_IMPLEMENTED,
        "<div class=\"banner info\">Inline edit lands in a follow-up. For now, delete + recreate.</div>",
    )
        .into_response()
}

pub async fn library_delete(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
) -> Response {
    let auth = crate::auth::AuthUser(admin.claims.clone());
    match crate::handlers::library::delete(State(state.clone()), auth, Path(id)).await {
        Ok(_) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], "").into_response(),
        Err(err) => (
            err.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                "<div class=\"banner error\">{}</div>",
                escape_html(&err.message)
            ),
        )
            .into_response(),
    }
}
