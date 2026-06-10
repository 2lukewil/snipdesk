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
const SETUP: &str = include_str!("templates/setup.html");

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

/// Render a full page wrapped in the layout. The `*_ACTIVE` slots gate
/// nav-link highlighting; pass `true` for the current page, `false`
/// for the others. Each handler hands in its own active-state via the
/// `NavTab` helper just below to keep the call sites readable.
async fn render_page(
    state: &AppState,
    session: &DashboardSession,
    title: &str,
    active: NavTab,
    content: &str,
) -> Html<String> {
    let (display, role) = fetch_nav_user(state, &session.claims).await;
    let update_banner = render_update_banner(state).await;
    Html(render(
        LAYOUT,
        &[
            ("TITLE", title),
            ("BRAND_NAME", &escape_html(&state.brand_name)),
            ("UPDATE_BANNER", &update_banner),
            (
                "USERS_ACTIVE",
                if matches!(active, NavTab::Users) {
                    "active"
                } else {
                    ""
                },
            ),
            (
                "LIBRARY_ACTIVE",
                if matches!(active, NavTab::Library) {
                    "active"
                } else {
                    ""
                },
            ),
            (
                "STATS_ACTIVE",
                if matches!(active, NavTab::Stats) {
                    "active"
                } else {
                    ""
                },
            ),
            (
                "AUDIT_ACTIVE",
                if matches!(active, NavTab::Audit) {
                    "active"
                } else {
                    ""
                },
            ),
            ("NAV_USER", &escape_html(&display)),
            ("NAV_ROLE", &escape_html(&role)),
            ("CONTENT", content),
        ],
    ))
}

/// Build the "newer release available" banner that sits between the
/// nav and the main content. Returns an empty string when no
/// update is known (either the poller hasn't completed a cycle or
/// the latest matches the running version). The banner links
/// straight to the release page so an operator gets the notes in
/// one click.
async fn render_update_banner(state: &AppState) -> String {
    let status = state.update_cache.current().await;
    if !status.is_newer {
        return String::new();
    }
    let version = status.latest_version.as_deref().unwrap_or("");
    let url = status.html_url.as_deref().unwrap_or("");
    let current = env!("CARGO_PKG_VERSION");
    format!(
        "<div class=\"update-banner\">\
           <span><strong>Update available:</strong> {brand} server {ver} (running {cur})</span> \
           <a href=\"{url}\" target=\"_blank\" rel=\"noopener\">View release notes &rarr;</a>\
         </div>",
        brand = escape_html(&state.brand_name),
        ver = escape_html(version),
        cur = escape_html(current),
        url = escape_html(url),
    )
}

/// Which nav-tab a page should highlight. `None` is for pages that
/// don't fit any tab cleanly (404, member-blocked, etc.); they get
/// the layout but no highlighted link.
#[derive(Copy, Clone)]
#[allow(dead_code)] // None is a placeholder for future pages
enum NavTab {
    Users,
    Library,
    Stats,
    Audit,
    None,
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
    State(state): State<AppState>,
    Query(q): Query<IndexQuery>,
    jar: CookieJar,
) -> Response {
    // First-run setup: a fresh database has no accounts and therefore
    // nothing to log in AS. Render the create-first-admin form instead
    // of a login form nobody can pass. The check is one COUNT on an
    // indexed table; once a single user exists this branch never runs
    // again.
    if user_count(&state).await == Some(0) {
        return render_setup_page(&state, q.error.as_deref()).into_response();
    }
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
        "<div class=\"banner info\">Your session expired. Sign in again.</div>".to_string()
    } else if q.error.as_deref() == Some("invalid") {
        "<div class=\"banner error\">Invalid email or password.</div>".to_string()
    } else if q.error.as_deref() == Some("disabled") {
        "<div class=\"banner error\">Your account is disabled. Contact your administrator.</div>"
            .to_string()
    } else if q.error.as_deref() == Some("signin") {
        "<div class=\"banner error\">Sign-in failed. Try again or contact your administrator.</div>"
            .to_string()
    } else {
        String::new()
    };
    let redirect_to = safe_next(q.redirect_to.as_deref());
    let sso_buttons = render_dashboard_sso_buttons(&state, &redirect_to);
    Html(render(
        LOGIN,
        &[
            ("BANNER", &banner),
            ("BRAND_NAME", &escape_html(&state.brand_name)),
            ("REDIRECT_TO", &escape_html(&redirect_to)),
            ("SSO_BUTTONS", &sso_buttons),
        ],
    ))
    .into_response()
}

/// Render the SSO button stack that appears under the password
/// form on the login page. Emits empty when no OIDC provider is
/// configured so the password form looks unchanged on
/// password-only deployments. The button targets the dashboard
/// SSO start URL (`/dashboard/oidc/<id>/start`), which 302s to
/// the IdP and rides the same callback as the desktop flow.
fn render_dashboard_sso_buttons(state: &AppState, redirect_to: &str) -> String {
    let providers = configured_dashboard_sso(state);
    if providers.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<div class=\"sso-section\">\
           <div class=\"sso-divider\"><span>or</span></div>\
           <div class=\"sso-buttons\">",
    );
    let rt = escape_html(redirect_to);
    for (id, label) in providers {
        out.push_str(&format!(
            "<a class=\"sso-button\" href=\"/dashboard/oidc/{id}/start?redirect_to={rt}\">{label}</a>",
            id = escape_html(&id),
            rt = rt,
            label = escape_html(&label),
        ));
    }
    out.push_str("</div></div>");
    out
}

/// Configured OIDC providers eligible for dashboard SSO, in the
/// order they should appear under the password form. Mirrors what
/// `/api/auth/methods` returns; the dashboard renders server-side
/// so we don't go through the JSON endpoint.
fn configured_dashboard_sso(state: &AppState) -> Vec<(String, String)> {
    let mut providers = Vec::new();
    if state.oidc_google.is_some() {
        providers.push(("google".to_string(), "Sign in with Google".to_string()));
    }
    if let Some(kc) = state.oidc_keycloak.as_ref() {
        let label = kc
            .display_name
            .clone()
            .unwrap_or_else(|| "Sign in with SSO".to_string());
        providers.push(("keycloak".to_string(), label));
    }
    providers
}

// ---- /dashboard/oidc/:provider/start (GET) ----

#[derive(Debug, Deserialize)]
pub struct DashboardOidcStartQuery {
    #[serde(default)]
    redirect_to: Option<String>,
}

/// Dashboard SSO entry. Resolves the provider segment and hands
/// off to the OIDC core with a Dashboard flow origin; the IdP-side
/// redirect URI stays the same as the desktop flow so operators
/// only register one callback URL per provider. Any failure
/// (unknown provider, IdP disabled in config, build_client error)
/// surfaces as a 302 back to the login page with `?error=signin`
/// so the user sees a clear retry path instead of a stack trace.
pub async fn dashboard_oidc_start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<DashboardOidcStartQuery>,
) -> Response {
    let redirect_to = safe_next(q.redirect_to.as_deref());
    let Some(provider) = crate::handlers::oidc::provider_from_id(&provider) else {
        return Redirect::to("/?error=signin").into_response();
    };
    match crate::handlers::oidc::dashboard_start(state, provider, redirect_to).await {
        Ok(resp) => resp,
        Err(_) => Redirect::to("/?error=signin").into_response(),
    }
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
    let jar = jar.add(build_cookie(token, state.secure_cookies));
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

// ---- First-run setup (/ when zero users, POST /dashboard/setup) ----

/// Total accounts on the server. None on a DB error - callers treat
/// that as "not first-run" so a transient failure can't surface the
/// setup form on a populated server.
async fn user_count(state: &AppState) -> Option<i64> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await
        .ok()
}

/// Render the create-first-admin page. `error` comes back through the
/// `?error=` query param after a rejected submit so the form can show
/// what to fix.
fn render_setup_page(state: &AppState, error: Option<&str>) -> Html<String> {
    let banner = match error {
        Some("setup_email") => {
            "<div class=\"banner error\">That doesn't look like an email address.</div>"
        }
        Some("setup_password") => {
            "<div class=\"banner error\">Password must be at least 10 characters.</div>"
        }
        Some("setup_name") => "<div class=\"banner error\">Enter a display name.</div>",
        Some("setup_failed") => {
            "<div class=\"banner error\">Setup failed. Check the server logs and try again.</div>"
        }
        _ => "",
    };
    Html(render(
        SETUP,
        &[
            ("BANNER", banner),
            ("BRAND_NAME", &escape_html(&state.brand_name)),
        ],
    ))
}

#[derive(Debug, Deserialize)]
pub struct SetupForm {
    pub display_name: String,
    pub email: String,
    pub password: String,
}

/// Create the first admin account from the setup form. Mirrors the
/// JSON signup handler's validation; the INSERT is guarded by a
/// zero-users predicate evaluated inside the statement itself, so two
/// racing submits (or a submit racing a desktop-client signup) can't
/// both land - SQLite runs the whole INSERT under one write lock and
/// the loser's `rows_affected` comes back 0.
pub async fn setup_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(body): Form<SetupForm>,
) -> Response {
    let email = body.email.trim().to_lowercase();
    let display_name = body.display_name.trim().to_string();
    if !crate::handlers::auth::looks_like_email(&email) {
        return Redirect::to("/?error=setup_email").into_response();
    }
    if body.password.len() < crate::handlers::auth::MIN_PASSWORD_LEN {
        return Redirect::to("/?error=setup_password").into_response();
    }
    if display_name.is_empty() {
        return Redirect::to("/?error=setup_name").into_response();
    }

    let password_hash = match hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("setup: hash_password failed: {e}");
            return Redirect::to("/?error=setup_failed").into_response();
        }
    };
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    // INSERT-if-empty: the SELECT runs under the INSERT's own write
    // lock, so this either creates the very first row (as admin) or
    // does nothing because someone else got there first.
    let result = sqlx::query(
        "INSERT INTO users \
           (id, email, display_name, role, is_disabled, created_at, last_seen_at, password_hash) \
         SELECT ?, ?, ?, 'admin', 0, ?, ?, ? \
         WHERE (SELECT COUNT(*) FROM users) = 0",
    )
    .bind(&id)
    .bind(&email)
    .bind(&display_name)
    .bind(now)
    .bind(now)
    .bind(&password_hash)
    .execute(&state.pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => {
            // Lost the race: an account now exists. Send them to the
            // login form (the index handler will no longer show setup).
            return Redirect::to("/").into_response();
        }
        Err(e) => {
            tracing::error!("setup: insert failed: {e}");
            return Redirect::to("/?error=setup_failed").into_response();
        }
    }

    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(&id),
            actor_email: &email,
            action: crate::audit::action::USER_CREATE,
            target_kind: Some("user"),
            target_id: Some(&id),
            details: Some(serde_json::json!({
                "via": "dashboard_setup",
                "email": email,
                "role": "admin",
            })),
        },
    )
    .await;
    tracing::info!(user_id = %id, email = %email, "first admin created via dashboard setup");

    // Sign them straight in: same cookie the login form would set.
    let token = match issue_token(&id, "admin", &state.jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("setup: issue_token failed: {e}");
            // The account exists; the login form works from here.
            return Redirect::to("/").into_response();
        }
    };
    let jar = jar.add(build_cookie(token, state.secure_cookies));
    (jar, Redirect::to("/dashboard/users")).into_response()
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
                NavTab::Users,
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

    render_page(&state, &session, "Users", NavTab::Users, &body)
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

// ---- /dashboard/users/:id (detail page) ----

/// Full-page profile view for a single user. Shows everything the
/// users list shows plus the user id (handy for support / log
/// correlation), action buttons mirroring the row-level controls, and
/// a small activity summary. Deliberately does NOT expose snippet
/// content; the encryption-at-rest posture says admins see counts +
/// metadata, never bodies.
pub async fn user_detail_page(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.created_at, u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         WHERE u.id = ? \
         GROUP BY u.id",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let row = match row {
        Some(r) => r,
        None => {
            return render_page(
                &state,
                &session,
                "User",
                NavTab::Users,
                "<div class=\"banner error\">No user with that id.</div>\
                 <p><a href=\"/dashboard/users\">Back to users list</a></p>",
            )
            .await
            .into_response();
        }
    };

    // Tombstones AND a per-table breakdown so an admin investigating a
    // suspicious "this user has way more snippets than I expected" can
    // see if the count is mostly deleted-but-not-purged rows.
    let live = row.snippet_count;
    let tombstoned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM personal_snippets WHERE owner_id = ? AND is_deleted = 1",
    )
    .bind(&row.id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // Paste telemetry for this specific user. Same per-user wpm/wage
    // override logic as the stats page, scoped to one row.
    let telemetry: (i64, i64, Option<i64>, Option<f64>, Option<String>) = sqlx::query_as(
        "SELECT chars_pasted, snippets_pasted, wpm, hourly_wage, currency \
         FROM users WHERE id = ?1",
    )
    .bind(&row.id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((0, 0, None, None, None));
    let (u_chars, u_pastes, u_wpm, u_wage, u_curr) = telemetry;
    let default_wpm = state.stats.wpm.max(1) as f64;
    let default_wage = state.stats.hourly_wage;
    let default_currency = &state.stats.currency;
    let wpm = u_wpm.map(|v| (v as f64).max(1.0)).unwrap_or(default_wpm);
    let wage = u_wage.unwrap_or(default_wage);
    let curr_used = u_curr.clone().unwrap_or_else(|| default_currency.clone());
    let rate = aud_rate_live(&state, &curr_used).await;
    let u_hours = u_chars as f64 / (wpm * 5.0 * 60.0);
    let u_money_aud = u_hours * wage * rate;

    // Top 3 library snippets this user reaches for. JOIN'd to
    // library_snippets so we have a title to show; tombstoned
    // library rows are excluded.
    let u_top_lib: Vec<(String, i64)> = sqlx::query_as(
        "SELECT s.title, lu.usage_count \
         FROM library_usage lu \
         JOIN library_snippets s ON s.id = lu.snippet_id AND s.is_deleted = 0 \
         WHERE lu.user_id = ?1 \
         ORDER BY lu.usage_count DESC LIMIT 3",
    )
    .bind(&row.id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let me_id = admin.user_id().to_string();
    let view = crate::handlers::admin::AdminUserView {
        id: row.id.clone(),
        email: row.email.clone(),
        display_name: row.display_name.clone(),
        role: row.role.clone(),
        is_disabled: row.is_disabled != 0,
        created_at: row.created_at,
        last_seen_at: row.last_seen_at,
        snippet_count: row.snippet_count,
    };

    let mut body = String::new();
    body.push_str("<p><a href=\"/dashboard/users\">&larr; Users</a></p>");
    body.push_str(&format!(
        "<h1>{name} <span class=\"pill role-{role}\">{role}</span></h1>",
        name = escape_html(&view.display_name),
        role = escape_html(&view.role),
    ));
    body.push_str("<div class=\"detail-grid\">");
    body.push_str(&detail_pair("Email", &escape_html(&view.email)));
    body.push_str(&detail_pair(
        "Account ID",
        &format!("<code>{}</code>", escape_html(&view.id)),
    ));
    body.push_str(&detail_pair(
        "Status",
        if view.is_disabled {
            "<span class=\"pill disabled\">disabled</span>"
        } else {
            "<span class=\"muted\">active</span>"
        },
    ));
    body.push_str(&detail_pair("Created", &format_relative(view.created_at)));
    let last_seen = match view.last_seen_at {
        None => "never".to_string(),
        Some(ts) => format_relative(ts),
    };
    body.push_str(&detail_pair("Last seen", &escape_html(&last_seen)));
    body.push_str(&detail_pair("Live snippets", &live.to_string()));
    body.push_str(&detail_pair(
        "Tombstoned (pending purge)",
        &tombstoned.to_string(),
    ));
    // Telemetry block. Only shown for users who've actually
    // reported something - otherwise we'd be adding seven "0" rows
    // to every freshly-signed-up user's page, which is noise.
    if u_chars > 0 || u_pastes > 0 {
        body.push_str(&detail_pair("Pastes", &format_thousands(u_pastes)));
        body.push_str(&detail_pair("Characters", &format_thousands(u_chars)));
        body.push_str(&detail_pair("Hours saved", &format!("{u_hours:.1}")));
        body.push_str(&detail_pair(
            "Money saved",
            &format!(
                "A${money} <span class=\"muted small\">({wpm:.0} wpm \u{00b7} {wage:.2} {curr}/hr)</span>",
                money = format_thousands(u_money_aud as i64),
                wpm = wpm,
                wage = wage,
                curr = escape_html(&curr_used),
            ),
        ));
        if !u_top_lib.is_empty() {
            let list: Vec<String> = u_top_lib
                .iter()
                .map(|(t, n)| {
                    format!(
                        "{}&nbsp;<span class=\"muted small\">x{}</span>",
                        escape_html(t),
                        n
                    )
                })
                .collect();
            body.push_str(&detail_pair("Favourite library", &list.join("; ")));
        }
    }
    body.push_str("</div>");

    // Action panel. Hidden for the current user (server-side gates
    // self-disable/demote/delete anyway, but better not to show
    // buttons that will always 400).
    if view.id != me_id {
        body.push_str("<h2 style=\"margin-top:24px\">Actions</h2>");
        let toggle_role_target = if view.role == "admin" {
            "member"
        } else {
            "admin"
        };
        let toggle_role_label = if view.role == "admin" {
            "Demote to member"
        } else {
            "Promote to admin"
        };
        let (disable_flag, disable_label) = if view.is_disabled {
            (false, "Re-enable account")
        } else {
            (true, "Disable account")
        };
        body.push_str(&format!(
            "<div class=\"detail-actions\">\
               <button class=\"btn\" \
                  hx-put=\"/dashboard/users/{id}\" \
                  hx-vals='{{\"role\":\"{role}\"}}' \
                  hx-swap=\"none\" \
                  hx-on::after-request=\"if(event.detail.successful) window.location.reload()\">{role_label}</button>\
               <button class=\"btn\" \
                  hx-put=\"/dashboard/users/{id}\" \
                  hx-vals='{{\"is_disabled\":{flag}}}' \
                  hx-swap=\"none\" \
                  hx-on::after-request=\"if(event.detail.successful) window.location.reload()\">{disable_label}</button>\
               <button class=\"btn danger\" \
                  hx-delete=\"/dashboard/users/{id}\" \
                  hx-confirm=\"Permanently delete {name}? Their personal snippets are removed from the server.\" \
                  hx-swap=\"none\" \
                  hx-on::after-request=\"if(event.detail.successful) window.location.href = '/dashboard/users'\">Delete account</button>\
             </div>",
            id = escape_html(&view.id),
            role = toggle_role_target,
            role_label = toggle_role_label,
            flag = disable_flag,
            disable_label = disable_label,
            name = escape_html(&view.display_name),
        ));
    } else {
        body.push_str("<p class=\"muted\" style=\"margin-top:20px\"><em>This is you. Self-targeted actions (disable, demote, delete) are blocked server-side.</em></p>");
    }

    render_page(&state, &session, "User", NavTab::Users, &body)
        .await
        .into_response()
}

fn detail_pair(label: &str, value: &str) -> String {
    format!(
        "<div class=\"detail-row\"><span class=\"detail-label\">{}</span><span class=\"detail-value\">{}</span></div>",
        escape_html(label),
        value,
    )
}

// ---- /dashboard/stats ----

#[derive(sqlx::FromRow)]
struct StatsCounts {
    total_users: i64,
    active_users: i64,
    admin_users: i64,
    disabled_users: i64,
    total_snippets: i64,
    tombstoned_snippets: i64,
    library_snippets: i64,
}

#[derive(sqlx::FromRow)]
struct RecentSignup {
    display_name: String,
    email: String,
    role: String,
    created_at: i64,
}

#[derive(sqlx::FromRow)]
struct RecentLibrary {
    title: String,
    folder_path: Option<String>,
    updated_at: i64,
}

pub async fn stats_page(State(state): State<AppState>, admin: DashboardAdmin) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };

    // One query rolls every count we need; the WHERE clauses are
    // cheap on the small datasets this serves and the round-trip
    // matters more than the rows scanned at this scale. Window for
    // "active" is 30 days, mirroring the typical "MAU" intuition.
    let cutoff_30d = chrono::Utc::now().timestamp() - 30 * 86_400;
    let counts: StatsCounts = sqlx::query_as(
        "SELECT \
            (SELECT COUNT(*) FROM users) AS total_users, \
            (SELECT COUNT(*) FROM users WHERE last_seen_at IS NOT NULL AND last_seen_at >= ?) AS active_users, \
            (SELECT COUNT(*) FROM users WHERE role = 'admin') AS admin_users, \
            (SELECT COUNT(*) FROM users WHERE is_disabled = 1) AS disabled_users, \
            (SELECT COUNT(*) FROM personal_snippets WHERE is_deleted = 0) AS total_snippets, \
            (SELECT COUNT(*) FROM personal_snippets WHERE is_deleted = 1) AS tombstoned_snippets, \
            (SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0) AS library_snippets",
    )
    .bind(cutoff_30d)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(StatsCounts {
        total_users: 0,
        active_users: 0,
        admin_users: 0,
        disabled_users: 0,
        total_snippets: 0,
        tombstoned_snippets: 0,
        library_snippets: 0,
    });

    let recent_users: Vec<RecentSignup> = sqlx::query_as(
        "SELECT display_name, email, role, created_at FROM users \
         ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let recent_library: Vec<RecentLibrary> = sqlx::query_as(
        "SELECT title, folder_path, updated_at FROM library_snippets \
         WHERE is_deleted = 0 ORDER BY updated_at DESC LIMIT 10",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    // Real telemetry. Aggregate every user's reported chars_pasted
    // and compute hours/money saved using each user's own wpm/wage
    // when they've set personal numbers, falling back to the
    // [stats] block in the server config otherwise. Until the
    // desktop client starts reporting (or for users who have it
    // disabled), the totals stay at zero - that's an honest
    // "no data yet" signal, not a regression.
    //
    // We pull every user row and do the per-user maths in Rust
    // because the FX table lives in `state.stats.aud_rates`, not in
    // SQLite. The query is COUNT(*)-bounded and runs once per page
    // load - fine even at thousands of users.
    let default_wpm = state.stats.wpm.max(1) as f64;
    let default_wage = state.stats.hourly_wage;
    let default_currency = state.stats.currency.clone();
    let default_rate = aud_rate_live(&state, &default_currency).await;

    let user_rows: Vec<UserStatsRow> =
        sqlx::query_as("SELECT chars_pasted, wpm, hourly_wage, currency FROM users")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();

    let mut total_chars_pasted: i64 = 0;
    let mut hours_saved = 0.0_f64;
    let mut money_saved_aud = 0.0_f64;
    for (chars, u_wpm, u_wage, u_curr) in &user_rows {
        if *chars <= 0 {
            continue;
        }
        total_chars_pasted += chars;
        let wpm = u_wpm.map(|v| (v as f64).max(1.0)).unwrap_or(default_wpm);
        let wage = u_wage.unwrap_or(default_wage);
        let rate = match u_curr.as_deref() {
            Some(c) => aud_rate_live(&state, c).await,
            None => default_rate,
        };
        let h = *chars as f64 / (wpm * 5.0 * 60.0);
        hours_saved += h;
        money_saved_aud += h * wage * rate;
    }
    let total_snippets_pasted: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(snippets_pasted), 0) FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);

    // Top contributors by paste activity. LIMIT 5 - the card slot
    // is narrow and the long tail isn't useful here.
    let top_users: Vec<(String, i64)> = sqlx::query_as(
        "SELECT display_name, chars_pasted FROM users \
         WHERE chars_pasted > 0 \
         ORDER BY chars_pasted DESC LIMIT 5",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    // Top library snippets by team-wide paste count. JOINs the
    // aggregate counter to the live row so we have the title to
    // display. ORDER BY total DESC, LIMIT 5.
    let top_library: Vec<(String, i64)> = sqlx::query_as(
        "SELECT s.title, COALESCE(SUM(u.usage_count), 0) AS total \
         FROM library_snippets s \
         LEFT JOIN library_usage u ON u.snippet_id = s.id \
         WHERE s.is_deleted = 0 \
         GROUP BY s.id \
         HAVING total > 0 \
         ORDER BY total DESC LIMIT 5",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    // Activity windows for the library: how many new snippets in
    // the last 7 / 30 days. Helps spot a stagnant library vs an
    // actively-growing one.
    let now = chrono::Utc::now().timestamp();
    let cutoff_7d = now - 7 * 86_400;
    let library_new_7d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0 AND created_at >= ?1",
    )
    .bind(cutoff_7d)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);
    let library_new_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_snippets WHERE is_deleted = 0 AND created_at >= ?1",
    )
    .bind(cutoff_30d)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // Adoption: percentage of users who've actually pasted at
    // least once. Cheap signal for "is rollout sticking".
    let adopters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE chars_pasted > 0")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let adoption_pct = if counts.total_users > 0 {
        (adopters as f64 / counts.total_users as f64 * 100.0).round() as i64
    } else {
        0
    };

    // Build the rate table the dashboard JS uses to reweight the
    // money card into the admin's chosen display currency. Live
    // values overlay the static table - same precedence the math
    // above used. Sorted by code so the dropdown is alphabetical
    // and stable across page renders.
    let mut display_rates: std::collections::BTreeMap<String, f64> =
        state.stats.aud_rates.clone().into_iter().collect();
    for (k, v) in state.fx_cache.rates.read().await.iter() {
        display_rates.insert(k.clone(), *v);
    }
    // AUD is the canonical base - always 1.0 - so we don't depend
    // on the provider returning it.
    display_rates.insert("AUD".to_string(), 1.0);
    let rates_json = serde_json::to_string(&display_rates).unwrap_or_else(|_| "{}".to_string());
    let codes: Vec<&str> = display_rates.keys().map(|s| s.as_str()).collect();

    let mut body = String::new();
    body.push_str("<h1>Server stats</h1>");
    body.push_str(
        "<p class=\"muted\">Activity snapshot. Click the &times; on any card to hide it; \
         use <strong>+ Add card</strong> to show ones you've hidden. Choices remember per browser; \
         <a href=\"#\" id=\"stats-reset\">Reset to defaults</a>.</p>",
    );
    // Toolbar: + Add picker on the left + currency dropdown on the
    // right. The Add menu is populated by JS from cards currently
    // hidden so it reflects whichever ones the admin can bring back.
    body.push_str("<div class=\"stats-toolbar\">");
    body.push_str(
        "<details class=\"stats-add\" id=\"stats-add\">\
           <summary>+ Add card</summary>\
           <div class=\"stats-add-menu\" id=\"stats-add-menu\"></div>\
         </details>",
    );
    body.push_str("<label class=\"stats-currency\"><span>Display currency:</span>");
    body.push_str("<select id=\"stats-currency-select\">");
    for c in &codes {
        body.push_str(&format!(
            "<option value=\"{c}\">{c}</option>",
            c = escape_html(c)
        ));
    }
    body.push_str("</select></label></div>");

    body.push_str("<div class=\"stats-grid\" id=\"stats-grid\">");
    body.push_str(&stat_card(
        "users",
        "Users",
        &counts.total_users.to_string(),
        "total accounts",
    ));
    body.push_str(&stat_card(
        "active",
        "Active (30 days)",
        &counts.active_users.to_string(),
        "last_seen in the last 30 days",
    ));
    body.push_str(&stat_card(
        "admins",
        "Admins",
        &counts.admin_users.to_string(),
        "users with the admin role",
    ));
    body.push_str(&stat_card(
        "disabled",
        "Disabled",
        &counts.disabled_users.to_string(),
        "blocked from signing in",
    ));
    body.push_str(&stat_card(
        "personal",
        "Personal snippets",
        &counts.total_snippets.to_string(),
        "live rows, encrypted at rest",
    ));
    body.push_str(&stat_card(
        "tombstones",
        "Tombstones",
        &counts.tombstoned_snippets.to_string(),
        "deleted, awaiting purge",
    ));
    body.push_str(&stat_card(
        "library",
        "Library snippets",
        &counts.library_snippets.to_string(),
        "shared with every member",
    ));
    body.push_str(&stat_card(
        "library_new_7d",
        "Library added (7d)",
        &library_new_7d.to_string(),
        "new shared snippets in the last week",
    ));
    body.push_str(&stat_card(
        "library_new_30d",
        "Library added (30d)",
        &library_new_30d.to_string(),
        "new shared snippets in the last 30 days",
    ));
    body.push_str(&stat_card(
        "adoption",
        "Adoption",
        &format!("{adoption_pct}%"),
        "users who've pasted at least once",
    ));
    body.push_str(&stat_card(
        "pastes",
        "Total pastes",
        &format_thousands(total_snippets_pasted),
        "snippet expansions across the team",
    ));
    body.push_str(&stat_card(
        "chars",
        "Characters pasted",
        &format_thousands(total_chars_pasted),
        "characters users didn't have to type",
    ));
    let curr = &state.stats.currency;
    body.push_str(&stat_card(
        "hours",
        "Hours saved",
        &format!("{hours_saved:.1}"),
        "computed from each user's wpm + paste totals",
    ));
    // Money card carries a data-aud attribute so the dropdown JS
    // can reweight the displayed value into any other code without
    // a server round-trip. The hint references the default
    // currency so operators understand the "raw" value before
    // they pick a different display code.
    body.push_str(&format!(
        "<div class=\"stat-card stat-card-money\" data-card-id=\"money\" data-aud=\"{aud}\">\
           <button type=\"button\" class=\"stat-close\" aria-label=\"Hide card\">&times;</button>\
           <div class=\"stat-value\" id=\"stat-money-value\">A${val}</div>\
           <div class=\"stat-label\">Money saved</div>\
           <div class=\"stat-hint\">each user's wage applied to their own pastes, displayed in {curr_safe} (default {default_safe})</div>\
         </div>",
        aud = money_saved_aud,
        val = format_thousands(money_saved_aud as i64),
        curr_safe = "<span id=\"stat-money-curr\">AUD</span>",
        default_safe = escape_html(curr),
    ));
    body.push_str("</div>");

    // Two narrow lists: top contributors + top library snippets.
    // Sized like the recent-grid so they sit on the same row.
    if !top_users.is_empty() || !top_library.is_empty() {
        body.push_str("<div class=\"recent-grid\">");
        body.push_str("<div><h2>Top contributors</h2>");
        if top_users.is_empty() {
            body.push_str("<p class=\"muted\">No paste activity reported yet.</p>");
        } else {
            body.push_str("<ul class=\"recent-list\">");
            for (name, chars) in &top_users {
                body.push_str(&format!(
                    "<li><strong>{name}</strong><br />\
                     <span class=\"muted small\">{chars} chars pasted</span></li>",
                    name = escape_html(name),
                    chars = format_thousands(*chars),
                ));
            }
            body.push_str("</ul>");
        }
        body.push_str("</div>");

        body.push_str("<div><h2>Top library snippets</h2>");
        if top_library.is_empty() {
            body.push_str("<p class=\"muted\">No library paste activity reported yet.</p>");
        } else {
            body.push_str("<ul class=\"recent-list\">");
            for (title, total) in &top_library {
                body.push_str(&format!(
                    "<li><strong>{title}</strong><br />\
                     <span class=\"muted small\">used {total} times across team</span></li>",
                    title = escape_html(title),
                    total = format_thousands(*total),
                ));
            }
            body.push_str("</ul>");
        }
        body.push_str("</div>");
        body.push_str("</div>");
    }

    body.push_str("<div class=\"recent-grid\">");
    body.push_str("<div><h2>Recent signups</h2>");
    if recent_users.is_empty() {
        body.push_str("<p class=\"muted\">No users yet.</p>");
    } else {
        body.push_str("<ul class=\"recent-list\">");
        for u in &recent_users {
            body.push_str(&format!(
                "<li><strong>{name}</strong> <span class=\"pill role-{role}\">{role}</span> \
                 <span class=\"muted small\">{when}</span><br />\
                 <span class=\"muted small\">{email}</span></li>",
                name = escape_html(&u.display_name),
                role = escape_html(&u.role),
                when = format_relative(u.created_at),
                email = escape_html(&u.email),
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str("</div>");

    body.push_str("<div><h2>Recent library snippets</h2>");
    if recent_library.is_empty() {
        body.push_str("<p class=\"muted\">No library snippets yet. Add one from the <a href=\"/dashboard/library\">library page</a>.</p>");
    } else {
        body.push_str("<ul class=\"recent-list\">");
        for s in &recent_library {
            let folder = s
                .folder_path
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(|p| format!(" <span class=\"muted small\">in {}</span>", escape_html(p)))
                .unwrap_or_default();
            body.push_str(&format!(
                "<li><strong>{title}</strong>{folder}<br />\
                 <span class=\"muted small\">updated {when}</span></li>",
                title = escape_html(&s.title),
                when = format_relative(s.updated_at),
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str("</div>");
    body.push_str("</div>");
    // Embed the rate table so the dropdown's JS can convert
    // money_saved_aud into any code locally. Kept inline rather
    // than on a separate endpoint so the dashboard stays
    // self-contained.
    body.push_str(&format!(
        "<script id=\"stats-rates\" type=\"application/json\">{rates_json}</script>"
    ));
    body.push_str(STATS_PAGE_JS);

    render_page(&state, &session, "Stats", NavTab::Stats, &body)
        .await
        .into_response()
}

fn stat_card(id: &str, label: &str, value: &str, hint: &str) -> String {
    // `data-card-id` lets the per-admin localStorage hide list refer
    // to cards stably across renders. The close button is wired up
    // by STATS_PAGE_JS below; on click it toggles the card's hidden
    // class and persists the id to localStorage so the choice
    // survives reloads.
    format!(
        "<div class=\"stat-card\" data-card-id=\"{id_safe}\">\
           <button type=\"button\" class=\"stat-close\" aria-label=\"Hide card\">&times;</button>\
           <div class=\"stat-value\">{value_safe}</div>\
           <div class=\"stat-label\">{label_safe}</div>\
           <div class=\"stat-hint\">{hint_safe}</div>\
         </div>",
        id_safe = escape_html(id),
        value_safe = escape_html(value),
        label_safe = escape_html(label),
        hint_safe = escape_html(hint),
    )
}

/// Per-user row used by the stats-page money/time aggregator.
/// `(chars_pasted, wpm_override, wage_override, currency_override)`.
/// `query_as` decodes tuple-of-Option directly from SELECT columns
/// in order, so the type is the schema.
type UserStatsRow = (i64, Option<i64>, Option<f64>, Option<String>);

/// Live-first AUD multiplier lookup. Consults the FX cache (populated
/// by `crate::fx::spawn_refresher` when `[fx]` is configured),
/// falling through to the static `stats.aud_rates` table for codes
/// not in the live feed, and finally to 1.0 (with a warn log) for
/// codes that nobody knows. Async because the cache lives behind a
/// `tokio::sync::RwLock`; the read lock is uncontended in steady
/// state (the refresher only takes the write lock when the periodic
/// fetch succeeds).
async fn aud_rate_live(state: &AppState, code: &str) -> f64 {
    crate::fx::rate_for(&state.fx_cache, &state.stats.aud_rates, code).await
}

/// Format an i64 with thousands separators. SQLite doesn't have
/// FORMAT() and pulling in a numeric-formatting crate for one call
/// would be silly. Used by stats cards that display large counts
/// (paste totals, library size, etc.).
fn format_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in bytes.iter().enumerate() {
        if i != 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*c as char);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Inline JS for the stats page: per-admin hide/show persistence in
/// localStorage. Doesn't need to coordinate with the server because
/// it's purely a display preference; storing it server-side would
/// also mean each admin's view is "their" view across browsers,
/// which is overkill for v1.
const STATS_PAGE_JS: &str = r#"<script>
(function () {
  // Switched the storage model from "hidden set" to "shown set".
  // Default visibility is a curated five (users, admins, hours,
  // money, adoption); anything else has to be added via the
  // + Add menu. A first-time admin sees a clean dashboard; power
  // users can grow it. The shown list lives in localStorage as
  // a JSON array of card-ids.
  var KEY = "snipdesk-shown-stat-cards-v2";
  var DEFAULT_SHOWN = ["users", "admins", "hours", "money", "adoption"];

  function loadShown() {
    try {
      var v = localStorage.getItem(KEY);
      if (v === null) return DEFAULT_SHOWN.slice();
      var parsed = JSON.parse(v);
      return Array.isArray(parsed) ? parsed : DEFAULT_SHOWN.slice();
    } catch (_e) { return DEFAULT_SHOWN.slice(); }
  }
  function saveShown(list) {
    try { localStorage.setItem(KEY, JSON.stringify(list)); } catch (_e) {}
  }

  // Scan every rendered card once so we know all available ids
  // and their human labels. The Add menu uses these.
  var ALL_CARDS = [];
  document.querySelectorAll(".stat-card[data-card-id]").forEach(function (el) {
    var id = el.getAttribute("data-card-id");
    var labelEl = el.querySelector(".stat-label");
    ALL_CARDS.push({ id: id, label: labelEl ? labelEl.textContent : id });
  });

  function applyShown() {
    var shown = loadShown();
    var shownSet = {};
    shown.forEach(function (id) { shownSet[id] = true; });
    document.querySelectorAll(".stat-card[data-card-id]").forEach(function (el) {
      el.classList.toggle("hidden", !shownSet[el.getAttribute("data-card-id")]);
    });
    rebuildAddMenu(shownSet);
  }

  function rebuildAddMenu(shownSet) {
    var menu = document.getElementById("stats-add-menu");
    if (!menu) return;
    menu.innerHTML = "";
    var any = false;
    ALL_CARDS.forEach(function (card) {
      if (shownSet[card.id]) return;
      any = true;
      var btn = document.createElement("button");
      btn.type = "button";
      btn.className = "stats-add-item";
      btn.setAttribute("data-add-id", card.id);
      btn.textContent = "+ " + card.label;
      menu.appendChild(btn);
    });
    if (!any) {
      var empty = document.createElement("div");
      empty.className = "stats-add-empty";
      empty.textContent = "All cards are already showing.";
      menu.appendChild(empty);
    }
  }

  applyShown();

  document.body.addEventListener("click", function (e) {
    var closeBtn = e.target.closest && e.target.closest(".stat-close");
    if (closeBtn) {
      var card = closeBtn.closest(".stat-card[data-card-id]");
      if (card) {
        var id = card.getAttribute("data-card-id");
        var shown = loadShown().filter(function (x) { return x !== id; });
        saveShown(shown);
        applyShown();
      }
      e.preventDefault();
      return;
    }
    var addBtn = e.target.closest && e.target.closest(".stats-add-item");
    if (addBtn) {
      var addId = addBtn.getAttribute("data-add-id");
      var shown = loadShown();
      if (shown.indexOf(addId) === -1) shown.push(addId);
      saveShown(shown);
      // Close the details panel so the menu collapses after a pick.
      var details = document.getElementById("stats-add");
      if (details) details.open = false;
      applyShown();
      e.preventDefault();
      return;
    }
    if (e.target && e.target.id === "stats-reset") {
      e.preventDefault();
      saveShown(DEFAULT_SHOWN.slice());
      applyShown();
    }
  });

  // ---- Currency dropdown ----
  // The Money saved card carries its raw value as data-aud="N". The
  // rate table is embedded as JSON in a sibling <script>; we read
  // it once at startup. Picking a code reweights the displayed
  // value: units_in_code = aud / aud_rates[code].
  var ratesEl = document.getElementById("stats-rates");
  var rates = {};
  try { rates = JSON.parse(ratesEl ? ratesEl.textContent : "{}"); }
  catch (_e) { rates = {}; }
  var sel = document.getElementById("stats-currency-select");
  var moneyEl = document.getElementById("stat-money-value");
  var moneyCurr = document.getElementById("stat-money-curr");
  var moneyCard = moneyEl ? moneyEl.closest(".stat-card") : null;
  var PREF_KEY = "snipdesk-stats-currency";

  function chosenCurrency() {
    var saved = null;
    try { saved = localStorage.getItem(PREF_KEY); } catch (_e) {}
    if (saved && rates[saved]) return saved;
    // First-time default: best-effort from navigator.language.
    // "en-US" -> USD, "de-DE" -> EUR, "ja-JP" -> JPY, etc.
    var localeMap = {
      "AU": "AUD", "US": "USD", "GB": "GBP", "DE": "EUR", "FR": "EUR",
      "IT": "EUR", "ES": "EUR", "NL": "EUR", "AT": "EUR", "BE": "EUR",
      "IE": "EUR", "PT": "EUR", "FI": "EUR", "GR": "EUR", "JP": "JPY",
      "CA": "CAD", "NZ": "NZD", "CH": "CHF", "IN": "INR", "SG": "SGD",
      "HK": "HKD", "ZA": "ZAR", "BR": "BRL", "MX": "MXN", "KR": "KRW",
      "SE": "SEK", "NO": "NOK", "DK": "DKK", "PL": "PLN", "CZ": "CZK",
      "TR": "TRY", "AE": "AED", "CN": "CNY", "TH": "THB", "ID": "IDR",
      "PH": "PHP",
    };
    var lang = (navigator.language || "").toUpperCase();
    var parts = lang.split(/[-_]/);
    var region = parts.length > 1 ? parts[1] : "";
    if (region && localeMap[region] && rates[localeMap[region]]) {
      return localeMap[region];
    }
    return "AUD";
  }

  function formatThousands(n) {
    return Math.round(n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  }

  function updateMoneyCard(code) {
    if (!moneyEl || !moneyCard) return;
    var aud = parseFloat(moneyCard.getAttribute("data-aud") || "0");
    var rate = rates[code];
    if (!rate || !isFinite(rate) || rate <= 0) return;
    var converted = aud / rate;
    // ISO-style "USD 1,234" for everything except the few codes
    // where a leading symbol is the norm. Keeps the column tidy
    // for any code we add later without a new entry per currency.
    // Symbols expressed as JS unicode escapes so the literal is pure
    // ASCII in the served HTML. Belt-and-braces against any layer
    // that might re-encode the response (proxies, browser legacy-
    // detection); the inline-script byte stream stays ASCII even
    // though the runtime string content is the right glyph.
    var symbol = ({"USD":"US$","AUD":"A$","CAD":"C$","NZD":"NZ$","HKD":"HK$","SGD":"S$","GBP":"\u00a3","EUR":"\u20ac","JPY":"\u00a5","CNY":"\u00a5","INR":"\u20b9"})[code];
    moneyEl.textContent = symbol
      ? symbol + formatThousands(converted)
      : code + " " + formatThousands(converted);
    if (moneyCurr) moneyCurr.textContent = code;
  }

  if (sel) {
    sel.value = chosenCurrency();
    updateMoneyCard(sel.value);
    sel.addEventListener("change", function () {
      try { localStorage.setItem(PREF_KEY, sel.value); } catch (_e) {}
      updateMoneyCard(sel.value);
    });
  }
})();
</script>"#;

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
         <td><a href=\"/dashboard/users/{id_attr}\">{name}</a></td>\
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
    admin: DashboardAdmin,
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
        id: id.clone(),
        email: email.clone(),
        display_name: display_name.clone(),
        role: role.to_string(),
        is_disabled: false,
        created_at: now,
        last_seen_at: None,
        snippet_count: 0,
    };

    // Audit the admin-created account so an operator can later see
    // which admin added a teammate. The signup JSON endpoint isn't
    // audited (it's the self-service path), so we record here, not
    // in handlers::auth::signup.
    let actor_email = crate::audit::lookup_actor_email(&state.pool, admin.user_id()).await;
    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(admin.user_id()),
            actor_email: &actor_email,
            action: crate::audit::action::USER_CREATE,
            target_kind: Some("user"),
            target_id: Some(&id),
            details: Some(serde_json::json!({
                "email": email,
                "role": role,
            })),
        },
    )
    .await;

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

pub async fn library_page(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Query(q): Query<LibraryPageQuery>,
) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };
    let rows = load_library(&state).await.unwrap_or_default();
    let selected = library_selected_folder(&q.folder);

    let mut body = String::new();
    body.push_str("<h1>Shared library</h1>");
    body.push_str(
        "<p class=\"muted\">Snippets here appear in every signed-in member's Team Library sidebar. \
         They're plaintext at rest - don't put secrets in.</p>",
    );
    // One-shot import-result banner (the import-confirm redirect
    // carries the counts in the query string).
    if let Some(imported) = q.imported {
        let skipped = q.skipped.unwrap_or(0);
        let detail = if skipped > 0 {
            format!(", skipped {skipped} (duplicates or errors)")
        } else {
            String::new()
        };
        body.push_str(&format!(
            "<div class=\"banner {}\">Imported {imported} snippet{}{detail}.</div>",
            if imported > 0 { "info" } else { "error" },
            if imported == 1 { "" } else { "s" },
        ));
    }
    body.push_str("<div class=\"library-layout\">");
    body.push_str(
        "<aside class=\"library-sidebar\" id=\"library-sidebar\" \
        hx-get=\"/dashboard/library/folders\" \
        hx-trigger=\"every 10s [document.querySelector('.lib-edit-form') === null], libraryChanged from:body, refresh-now\" \
        hx-swap=\"innerHTML\" hx-include=\"#library-folder-input\">",
    );
    let folders = load_library_folders(&state).await;
    body.push_str(&render_library_folder_tree(&rows, &folders, &selected));
    body.push_str("</aside>");
    body.push_str("<div class=\"library-main\">");
    // Hidden input mirrors the current folder so polling sweeps the
    // right view. htmx's hx-include picks it up and appends ?folder=.
    body.push_str(&format!(
        "<input type=\"hidden\" id=\"library-folder-input\" name=\"folder\" value=\"{}\" />",
        escape_html(&selected),
    ));
    body.push_str(&library_create_form(&selected));
    // Search + export/import toolbar. The search input re-fetches
    // the cards fragment as the admin types (debounced). Export and
    // Import both open the selection-tree modal: Export fetches the
    // picker fragment over the whole library; Import pops the file
    // browser directly and posts the file's text to the preview
    // endpoint. Wiring lives in LIBRARY_PAGE_JS.
    let q_value = q.q.as_deref().unwrap_or("");
    body.push_str(&format!(
        "<div class=\"library-toolbar\">\
           <input type=\"search\" id=\"library-search-input\" name=\"q\" value=\"{q}\" \
                  placeholder=\"Search title, body, tags\" autocomplete=\"off\" \
                  hx-get=\"/dashboard/library/cards\" \
                  hx-trigger=\"input changed delay:300ms, search\" \
                  hx-target=\"#library-list\" \
                  hx-include=\"#library-folder-input\" \
                  hx-swap=\"innerHTML\" />\
           <button class=\"btn\" id=\"library-export-btn\" type=\"button\">Export</button>\
           <button class=\"btn\" id=\"library-import-btn\" type=\"button\">Import</button>\
           <input type=\"file\" id=\"library-import-file\" accept=\".json,.csv\" hidden />\
         </div>",
        q = escape_html(q_value),
    ));
    // Selection-tree modal shell. Export and import fragments load
    // into #library-modal-body; behaviour is delegated from
    // LIBRARY_PAGE_JS so inserted markup needs no scripts of its own.
    body.push_str(
        "<div id=\"library-modal\" class=\"dash-modal\" hidden>\
           <div class=\"dash-modal-card\">\
             <button type=\"button\" class=\"dash-modal-close\" id=\"library-modal-close\" aria-label=\"Close\">&#215;</button>\
             <div id=\"library-modal-body\"></div>\
           </div>\
         </div>",
    );
    // Polls every 5s so another admin's adds / edits / deletes surface
    // without a manual refresh. The folder filter + search query ride
    // along via hx-include. The JS-expression gate on the trigger
    // skips the poll when an inline edit form is open, otherwise the
    // next tick would wipe whatever the admin was typing - the poll
    // swaps the whole tbody's innerHTML, including their
    // half-finished edit.
    body.push_str(
        "<div class=\"library-list\" id=\"library-list\" \
              hx-get=\"/dashboard/library/cards\" \
              hx-trigger=\"every 5s [document.querySelector('.lib-edit-form') === null], libraryChanged from:body, refresh-now\" \
              hx-include=\"#library-folder-input,#library-search-input\" \
              hx-swap=\"innerHTML\">",
    );
    let filtered = filter_library_query(filter_library_rows(&rows, &selected), &q.q);
    body.push_str(&render_library_cards_inner(&filtered));
    body.push_str("</div>");
    body.push_str("</div></div>");
    // Drag-drop + formatting-toolbar JS, scoped to the library page.
    // Inline so we don't have to ship another static asset.
    body.push_str(LIBRARY_PAGE_JS);

    render_page(&state, &session, "Library", NavTab::Library, &body)
        .await
        .into_response()
}

/// Pseudo-folder values used in the URL and the hx-include hidden
/// input. "__all__" means no folder filter (default); "__unfiled__"
/// shows snippets without a folder_path; anything else is a literal
/// folder path like "Billing/Refunds".
const FOLDER_ALL: &str = "__all__";
const FOLDER_UNFILED: &str = "__unfiled__";

fn library_selected_folder(raw: &Option<String>) -> String {
    match raw.as_deref().map(str::trim) {
        Some("") | None => FOLDER_ALL.to_string(),
        Some(s) => s.to_string(),
    }
}

/// Apply the same predicate the SQL would for a given folder selector.
/// Done in-memory because we already loaded every row for the sidebar
/// counts; one walk is cheaper than two queries for the small library
/// sizes this serves.
fn filter_library_rows<'a>(rows: &'a [LibraryRow], selected: &str) -> Vec<&'a LibraryRow> {
    rows.iter()
        .filter(|r| match selected {
            FOLDER_ALL => true,
            FOLDER_UNFILED => r.folder_path.as_deref().unwrap_or("").is_empty(),
            path => match &r.folder_path {
                Some(fp) if !fp.is_empty() => fp == path || fp.starts_with(&format!("{path}/")),
                _ => false,
            },
        })
        .collect()
}

/// Build the sidebar folder list as a nested tree. Pseudo-nodes
/// All + Unfiled live at the top; the actual folders render in
/// hierarchical order with indentation per slash-depth. Missing
/// parents are synthesised (so "A/B/C" implies A and A/B nodes
/// even if no snippet sits directly in them).
///
/// Counts are recursive: clicking a parent surfaces every
/// descendant's snippets (the filter at `filter_library_rows`
/// already matches `path` OR `path/...`), so the number we show
/// is the same number the user will see in the card list. The
/// previous "direct count" was confusing because the visible
/// total didn't match.
///
/// Each folder node carries:
///   - `data-folder-path` (full path) so snippet-drop knows where
///     to land
///   - `data-folder-source="1"` (full path again) so the folder
///     can itself be dragged for nest/unnest operations
///
/// A leading "root" drop zone lets the user drop a nested folder
/// onto it to lift it back to the top level.
/// (path, sort_order) - the shape read from library_folders for
/// passing into the tree renderer.
type FolderRow = (String, i64);

fn render_library_folder_tree(
    rows: &[LibraryRow],
    folders: &[FolderRow],
    selected: &str,
) -> String {
    // Collect every distinct folder_path that has at least one
    // snippet, plus every ancestor segment. BTreeSet keeps the
    // final iteration alphabetical, which is what the tree walk
    // wants. We then union in every path from library_folders so
    // empty (admin-created) folders also show up.
    let mut paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut unfiled = 0i64;
    let mut all = 0i64;
    for r in rows {
        all += 1;
        match r.folder_path.as_deref() {
            None | Some("") => unfiled += 1,
            Some(fp) => {
                // Add the path itself + every ancestor segment so
                // a deeply nested snippet brings its whole branch
                // into the sidebar even if mid-tree folders are
                // empty.
                let mut cursor = String::new();
                for seg in fp.split('/').filter(|s| !s.is_empty()) {
                    if !cursor.is_empty() {
                        cursor.push('/');
                    }
                    cursor.push_str(seg);
                    paths.insert(cursor.clone());
                }
            }
        }
    }
    // Union explicit folder rows (empty folders created via the
    // "+ New folder" button). Include their ancestors too.
    for (fp, _) in folders {
        let mut cursor = String::new();
        for seg in fp.split('/').filter(|s| !s.is_empty()) {
            if !cursor.is_empty() {
                cursor.push('/');
            }
            cursor.push_str(seg);
            paths.insert(cursor.clone());
        }
    }
    // Sort-order lookup. Defaults to 0 for paths that don't have
    // an explicit row (lazy-create hasn't caught up yet); the
    // JS-side sort uses path as the tiebreak, which collapses to
    // alphabetical.
    let sort_order: std::collections::HashMap<&str, i64> =
        folders.iter().map(|(p, o)| (p.as_str(), *o)).collect();

    // Recursive count = "snippets that show up if you click this
    // folder", matching the filter semantics. Cheap to compute:
    // for each path P, count rows where folder_path == P or
    // starts_with("P/").
    let count_for = |path: &str| -> i64 {
        rows.iter()
            .filter(|r| match r.folder_path.as_deref() {
                Some(fp) if !fp.is_empty() => fp == path || fp.starts_with(&format!("{path}/")),
                _ => false,
            })
            .count() as i64
    };

    // Has-children index, computed once: a folder has children
    // iff some other path in the set starts with its prefix +
    // "/". Drives the caret slot below.
    let has_children: std::collections::HashSet<String> = paths
        .iter()
        .filter(|p| {
            let pre = format!("{p}/");
            paths.iter().any(|q| q.starts_with(&pre))
        })
        .cloned()
        .collect();

    let mut out = String::new();
    out.push_str("<div class=\"lib-folder-header\">Folders</div>");
    // Sort-mode toggle. The actual ordering happens client-side
    // (JS reads the data-sort-order attribute and re-shuffles the
    // DOM); the server always emits alphabetical so the
    // first-paint is correct without JS, and the JS pass swaps
    // siblings into manual order if the admin has chosen that.
    out.push_str(
        "<div class=\"lib-folder-controls\">\
           <label class=\"lib-sort-mode\">\
             <span>Sort</span>\
             <select id=\"lib-sort-mode-select\">\
               <option value=\"alpha\">Alphabetical</option>\
               <option value=\"manual\">Manual</option>\
             </select>\
           </label>\
         </div>",
    );
    // Inline "+ New folder" form. Submits via JS-driven fetch so
    // we can clear the input + trigger the sidebar/cards refresh
    // without a full page reload. Path goes through the same
    // normalisation as the snippet save path on the server.
    out.push_str(
        "<form class=\"lib-folder-create\" id=\"lib-folder-create-form\">\
           <input type=\"text\" id=\"lib-folder-create-input\" \
                  placeholder=\"e.g. Replies/Refunds\" \
                  autocomplete=\"off\" spellcheck=\"false\" />\
           <button type=\"submit\" class=\"btn primary\">+ Folder</button>\
         </form>",
    );
    out.push_str(&render_lib_folder_node(FolderNodeArgs {
        path: FOLDER_ALL,
        label: "All snippets",
        count: all,
        active: selected == FOLDER_ALL,
        kind: FolderNodeKind::Special,
        depth: 0,
        has_children: false,
        sort_order: 0,
    }));
    out.push_str(&render_lib_folder_node(FolderNodeArgs {
        path: FOLDER_UNFILED,
        label: "Unfiled",
        count: unfiled,
        active: selected == FOLDER_UNFILED,
        kind: FolderNodeKind::Unfiled,
        depth: 0,
        has_children: false,
        sort_order: 0,
    }));
    // The root drop zone: dropping a folder here lifts it back to
    // top level. Drawn as a thin band between Unfiled and the
    // actual folders so it's discoverable without dominating.
    out.push_str(
        "<div class=\"lib-folder-root-drop\" data-folder-root-drop=\"1\">\
           <span class=\"label muted small\">drop here to un-nest</span>\
         </div>",
    );

    // Tree walk: each path renders at indent = depth (number of '/' segments).
    // BTreeSet iteration is alphabetical, which naturally yields
    // parents before children for any given branch.
    for path in &paths {
        let depth = path.matches('/').count();
        let label = path.rsplit('/').next().unwrap_or(path);
        out.push_str(&render_lib_folder_node(FolderNodeArgs {
            path,
            label,
            count: count_for(path),
            active: selected == path.as_str(),
            kind: FolderNodeKind::Real,
            depth,
            has_children: has_children.contains(path),
            sort_order: *sort_order.get(path.as_str()).unwrap_or(&0),
        }));
    }
    out
}

/// What kind of pseudo-node we're rendering. Affects droppability
/// (Special isn't a drop target; Unfiled accepts only snippet
/// drops; Real accepts both snippet drops and folder-DnD drops)
/// and draggability (only Real folders can themselves be dragged).
#[derive(Copy, Clone)]
enum FolderNodeKind {
    Special, // All-snippets pseudo-node, not droppable
    Unfiled, // Special drop target that maps to "" folder_path
    Real,    // A real folder path; full DnD on both axes
}

/// Args for `render_lib_folder_node`. Bundled into a struct so
/// the call sites stay readable - 8 positional args was tipping
/// over clippy's complexity bar.
struct FolderNodeArgs<'a> {
    path: &'a str,
    label: &'a str,
    count: i64,
    active: bool,
    kind: FolderNodeKind,
    depth: usize,
    has_children: bool,
    sort_order: i64,
}

fn render_lib_folder_node(args: FolderNodeArgs<'_>) -> String {
    let FolderNodeArgs {
        path,
        label,
        count,
        active,
        kind,
        depth,
        has_children,
        sort_order,
    } = args;
    let active_class = if active { " active" } else { "" };
    // CSS reads --depth to compute padding-left; keeps the rule
    // simple instead of generating a class per depth level.
    let style = if depth > 0 {
        format!(" style=\"--depth: {depth};\"")
    } else {
        String::new()
    };
    let (drop_attrs, drag_attrs) = match kind {
        FolderNodeKind::Special => ("", ""),
        FolderNodeKind::Unfiled => ("data-droppable=\"1\" data-unfiled=\"1\"", ""),
        FolderNodeKind::Real => (
            "data-droppable=\"1\"",
            // Folders are themselves draggable so a folder-drop
            // can pick them up for nest / unnest moves. The path
            // doubles as the source id. draggable lives on the row
            // wrapper (not the inner link) so the drag image
            // includes caret + label and clicks on the caret can
            // be cleanly cancelled.
            "draggable=\"true\" data-folder-source=\"1\"",
        ),
    };
    // Caret toggle in front of folders that have children. Leaves
    // and special pseudo-nodes get an empty spacer that takes the
    // same width so labels line up across the column.
    let caret_html = match kind {
        FolderNodeKind::Real if has_children => format!(
            "<span class=\"lib-folder-caret\" data-folder-caret=\"{p}\" \
             role=\"button\" aria-label=\"Collapse/expand folder\" \
             tabindex=\"0\">&#x25BE;</span>",
            p = escape_html(path),
        ),
        _ => "<span class=\"lib-folder-caret-spacer\" aria-hidden=\"true\"></span>".to_string(),
    };
    // Tree-arrow glyph for nested folders only. Depth-0 rows skip
    // the slot entirely - they don't need the structural hint and
    // the empty spacer was wasting horizontal real estate on every
    // top-level row.
    let tree_glyph = if depth > 0 {
        "<span class=\"lib-tree-glyph\" aria-hidden=\"true\">&#x21B3;</span>"
    } else {
        ""
    };
    // Folder row is now a <div> wrapping a separate caret + link.
    // Previous shape (caret span inside an <a>) made clicking the
    // caret unreliable: even with preventDefault, some browsers
    // still followed the link, and the draggable=true on the <a>
    // could swallow a click as a drag start. Splitting caret out
    // means the caret's click handler is the ONLY listener for
    // that span's click event.
    // No inner <a>. Previous revisions nested an
    // <a class="lib-folder-link"> inside the draggable <div>, and
    // browsers handle that ambiguity inconsistently - the spec
    // says the closest draggable ancestor wins, but in Chromium
    // the inner anchor's implicit drag-as-link behaviour wins for
    // every row AFTER the first draggable=true element on the
    // page. That manifested as "only the top-listed folder
    // drags," with nested folders coincidentally working because
    // they have a tree-glyph between caret and link that broke
    // the conflict pattern.
    //
    // Solution: drop the <a> entirely. The row is a div with a JS
    // click handler that navigates to /dashboard/library?folder=X
    // (data-folder-href carries the URL). Drag is unambiguous.
    // Ctrl/middle-click for new tab is lost, but folder
    // navigation in an admin sidebar doesn't lean on that the way
    // public docs do.
    let folder_href = format!("/dashboard/library?folder={}", escape_html(path));
    format!(
        "<div class=\"lib-folder-row{active_class}\" \
            data-folder-path=\"{path_attr}\" data-sort-order=\"{sort_order}\" \
            data-folder-href=\"{href_attr}\" \
            {drop_attrs} {drag_attrs}{style}>\
           {caret}{glyph}\
           <span class=\"label\">{label_safe}</span>\
           <span class=\"count\">{count}</span>\
         </div>",
        path_attr = escape_html(path),
        href_attr = escape_html(&folder_href),
        label_safe = escape_html(label),
        caret = caret_html,
        glyph = tree_glyph,
    )
}

/// Fragment endpoint: just the library cards (no outer container).
/// Hit by the polling tick on `/dashboard/library` and by the JS-driven
/// refresh after drag-drop. Honors the `folder` query param so polling
/// sweeps the current view, not the unfiltered list.
pub async fn library_cards(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Query(q): Query<LibraryPageQuery>,
) -> Response {
    let rows = load_library(&state).await.unwrap_or_default();
    let selected = library_selected_folder(&q.folder);
    let filtered = filter_library_query(filter_library_rows(&rows, &selected), &q.q);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_library_cards_inner(&filtered),
    )
        .into_response()
}

/// Fragment endpoint: just the sidebar folder list. Used by the
/// sidebar's 10s polling sweep so a new folder created elsewhere
/// shows up without a page reload.
pub async fn library_folders(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Query(q): Query<LibraryPageQuery>,
) -> Response {
    let rows = load_library(&state).await.unwrap_or_default();
    let folders = load_library_folders(&state).await;
    let selected = library_selected_folder(&q.folder);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_library_folder_tree(&rows, &folders, &selected),
    )
        .into_response()
}

/// Shared body of the cards container; same output whether we're
/// rendering the initial page or a polling refresh.
// ---- /dashboard/library/export (GET) ----

#[derive(Debug, Deserialize)]
pub struct LibraryExportQuery {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
}

/// The interchange shape shared with the desktop client: its JSON
/// importer accepts exactly this (`NewSnippet[]`), and its exporter
/// emits a superset of it. Tags travel as a plain array.
#[derive(serde::Serialize)]
struct ExportEntry {
    title: String,
    body: String,
    tags: Vec<String>,
    folder_path: Option<String>,
}

/// Decode the library's stored tags format (",tag1,tag2,") into a
/// plain list.
fn decode_tags(stored: &str) -> Vec<String> {
    stored
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// RFC-4180-ish CSV field escaping; mirrors the desktop client's
/// csv_field so files round-trip through either parser.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Serialize library rows into the requested download format.
/// JSON is the canonical interchange shape (the desktop client
/// imports it directly); CSV carries the same columns the client
/// reads, plus folder_path.
fn build_library_export(filtered: &[&LibraryRow], format: &str) -> (String, String, String) {
    let date = Utc::now().format("%Y%m%d");
    if format == "csv" {
        let mut out = String::from("title,body,tags,folder_path\n");
        for r in filtered {
            out.push_str(&format!(
                "{},{},{},{}\n",
                csv_field(&r.title),
                csv_field(&r.body),
                csv_field(&decode_tags(&r.tags).join(";")),
                csv_field(r.folder_path.as_deref().unwrap_or("")),
            ));
        }
        (
            "text/csv; charset=utf-8".to_string(),
            format!("library-{date}.csv"),
            out,
        )
    } else {
        let entries: Vec<ExportEntry> = filtered
            .iter()
            .map(|r| ExportEntry {
                title: r.title.clone(),
                body: r.body.clone(),
                tags: decode_tags(&r.tags),
                folder_path: r.folder_path.clone().filter(|f| !f.is_empty()),
            })
            .collect();
        let json = serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".into());
        (
            "application/json; charset=utf-8".to_string(),
            format!("library-{date}.json"),
            json,
        )
    }
}

/// Shared tail of both export handlers: audit, then answer with the
/// attachment.
async fn finish_library_export(
    state: &AppState,
    admin: &DashboardAdmin,
    filtered: &[&LibraryRow],
    format: &str,
    scope: serde_json::Value,
) -> Response {
    let (content_type, filename, payload) = build_library_export(filtered, format);
    // Count + scope in the audit trail, never content.
    let actor_email = crate::audit::lookup_actor_email(&state.pool, admin.user_id()).await;
    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(admin.user_id()),
            actor_email: &actor_email,
            action: crate::audit::action::LIBRARY_EXPORT,
            target_kind: Some("library"),
            target_id: None,
            details: Some(serde_json::json!({
                "count": filtered.len(),
                "format": format,
                "scope": scope,
            })),
        },
    )
    .await;

    (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        payload,
    )
        .into_response()
}

/// GET: download the library scoped by the search + folder query
/// params (kept for direct-URL use; the dashboard UI drives the
/// POST variant below via the selection modal).
pub async fn library_export(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Query(q): Query<LibraryExportQuery>,
) -> Response {
    let rows = load_library(&state).await.unwrap_or_default();
    let selected = library_selected_folder(&q.folder);
    let filtered = filter_library_query(filter_library_rows(&rows, &selected), &q.q);
    let format = q.format.as_deref().unwrap_or("json").to_string();
    let scope = serde_json::json!({
        "q": q.q.as_deref().unwrap_or(""),
        "folder": selected,
    });
    finish_library_export(&state, &admin, &filtered, &format, scope).await
}

#[derive(Debug, Deserialize)]
pub struct LibraryExportSelectedForm {
    #[serde(default)]
    pub format: Option<String>,
    /// JSON array of library snippet ids from the selection modal.
    pub selected: String,
}

/// POST: download exactly the snippets the selection modal checked.
pub async fn library_export_selected(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Form(form): Form<LibraryExportSelectedForm>,
) -> Response {
    let ids: std::collections::HashSet<String> =
        serde_json::from_str::<Vec<String>>(&form.selected)
            .unwrap_or_default()
            .into_iter()
            .collect();
    let rows = load_library(&state).await.unwrap_or_default();
    let filtered: Vec<&LibraryRow> = rows.iter().filter(|r| ids.contains(&r.id)).collect();
    let format = form.format.as_deref().unwrap_or("json").to_string();
    let scope = serde_json::json!({ "selection": "explicit" });
    finish_library_export(&state, &admin, &filtered, &format, scope).await
}

// ---- /dashboard/library/import (GET page, POST preview, POST confirm) ----

/// One entry from an uploaded file, normalized. Deserializes from
/// the interchange JSON (NewSnippet[] shape); the desktop client's
/// full Snippet[] export also lands here because serde ignores the
/// extra fields.
#[derive(Debug, serde::Serialize, Deserialize)]
struct ImportFileEntry {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    folder_path: Option<String>,
}

/// Parse the uploaded file. JSON when the trimmed content starts
/// like a JSON array/object, CSV otherwise. CSV mirrors the desktop
/// client's parser: RFC-4180-ish quoting, header-driven columns
/// (title + body required, tags + folder_path optional), tags split
/// on ';' or ','.
fn parse_import_file(content: &str) -> Result<Vec<ImportFileEntry>, String> {
    let trimmed = content.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return serde_json::from_str::<Vec<ImportFileEntry>>(content)
            .map_err(|e| format!("couldn't parse JSON: {e}"));
    }
    parse_import_csv(content)
}

fn parse_import_csv(contents: &str) -> Result<Vec<ImportFileEntry>, String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = contents.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => cur_row.push(std::mem::take(&mut cur)),
                '\n' => {
                    cur_row.push(std::mem::take(&mut cur));
                    rows.push(std::mem::take(&mut cur_row));
                }
                '\r' => {}
                _ => cur.push(c),
            }
        }
    }
    if !cur.is_empty() || !cur_row.is_empty() {
        cur_row.push(cur);
        rows.push(cur_row);
    }
    if rows.is_empty() {
        return Ok(vec![]);
    }

    let header = rows.remove(0);
    let find = |name: &str| {
        header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let title_idx = find("title").ok_or("CSV is missing a 'title' column")?;
    let body_idx = find("body").ok_or("CSV is missing a 'body' column")?;
    let tags_idx = find("tags");
    let folder_idx = find("folder_path");

    let mut out = Vec::new();
    for row in rows {
        if row.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let title = row.get(title_idx).cloned().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let tags = tags_idx
            .and_then(|i| row.get(i).cloned())
            .map(|s| {
                s.split([';', ','])
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let folder_path = folder_idx
            .and_then(|i| row.get(i).cloned())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        out.push(ImportFileEntry {
            title,
            body: row.get(body_idx).cloned().unwrap_or_default(),
            tags,
            folder_path,
        });
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct ImportPreviewForm {
    pub content: String,
}

/// POST /dashboard/library/import/preview - parse the file and
/// return the folder-tree selection FRAGMENT. The library page's JS
/// inserts it into the selection modal; all behaviour is delegated
/// there, so this markup carries no scripts. Stateless: the
/// normalized entries ride to the confirm step in a hidden field.
pub async fn library_import_preview(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Form(form): Form<ImportPreviewForm>,
) -> Response {
    let entries = match parse_import_file(&form.content) {
        Ok(e) if e.is_empty() => {
            return modal_fragment(
                "<div class=\"banner error\">No snippets found in that file.</div>".to_string(),
            );
        }
        Ok(e) => e,
        Err(msg) => {
            return modal_fragment(format!(
                "<div class=\"banner error\">{}</div>",
                escape_html(&msg)
            ));
        }
    };

    // Existing library titles, for duplicate badging (trimmed
    // lowercase title - the same rule the desktop importer uses).
    let existing: std::collections::HashSet<String> = load_library(&state)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.title.trim().to_lowercase())
        .collect();

    let payload_json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into());
    let items: Vec<TreeItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, e)| {
            let dup = existing.contains(&e.title.trim().to_lowercase());
            TreeItem {
                key: idx.to_string(),
                title: e.title.clone(),
                folder: e.folder_path.clone().unwrap_or_default(),
                checked: !dup,
                badge: dup,
            }
        })
        .collect();
    let tree = render_selection_tree(&items);

    modal_fragment(format!(
        "<h2>Import into the shared library</h2>\
         <p class=\"muted\">Everything new starts selected; duplicates of existing \
          library titles start deselected. Expand folders to cherry-pick.</p>\
         <div class=\"imp-toolbar\">\
           <label><input type=\"checkbox\" id=\"imp-master\" /> Select all</label>\
           <input type=\"search\" id=\"imp-search\" placeholder=\"Filter by title\" autocomplete=\"off\" />\
           <span class=\"muted\" id=\"imp-count\"></span>\
         </div>\
         <div id=\"imp-tree\">{tree}</div>\
         <form method=\"post\" action=\"/dashboard/library/import\" class=\"imp-form\" data-keys=\"numeric\">\
           <input type=\"hidden\" name=\"payload\" value=\"{payload}\" />\
           <input type=\"hidden\" name=\"selected\" />\
           <div class=\"imp-actions\">\
             <button type=\"submit\" class=\"primary imp-confirm\">Import selected</button>\
           </div>\
         </form>",
        tree = tree,
        payload = escape_html(&payload_json),
    ))
}

/// GET /dashboard/library/export/picker - the export half of the
/// selection modal: the whole library as a tree, everything
/// selected. Submitting posts the chosen ids to the export
/// endpoint, which answers with the file download.
pub async fn library_export_picker(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
) -> Response {
    let rows = load_library(&state).await.unwrap_or_default();
    if rows.is_empty() {
        return modal_fragment(
            "<div class=\"banner error\">The library is empty - nothing to export.</div>"
                .to_string(),
        );
    }
    let items: Vec<TreeItem> = rows
        .iter()
        .map(|r| TreeItem {
            key: r.id.clone(),
            title: r.title.clone(),
            folder: r.folder_path.clone().unwrap_or_default(),
            checked: true,
            badge: false,
        })
        .collect();
    let tree = render_selection_tree(&items);

    modal_fragment(format!(
        "<h2>Export the shared library</h2>\
         <p class=\"muted\">Everything starts selected. Untick folders or snippets \
          to leave them out, then pick a format.</p>\
         <div class=\"imp-toolbar\">\
           <label><input type=\"checkbox\" id=\"imp-master\" /> Select all</label>\
           <input type=\"search\" id=\"imp-search\" placeholder=\"Filter by title\" autocomplete=\"off\" />\
           <span class=\"muted\" id=\"imp-count\"></span>\
         </div>\
         <div id=\"imp-tree\">{tree}</div>\
         <form method=\"post\" action=\"/dashboard/library/export\" class=\"imp-form\" data-close-on-submit=\"1\">\
           <input type=\"hidden\" name=\"selected\" />\
           <div class=\"imp-actions\">\
             <button type=\"submit\" name=\"format\" value=\"json\" class=\"primary imp-confirm\">Export JSON</button>\
             <button type=\"submit\" name=\"format\" value=\"csv\" class=\"primary imp-confirm\">Export CSV</button>\
           </div>\
         </form>",
    ))
}

/// Wrap fragment HTML for insertion into the library page's modal.
fn modal_fragment(inner: String) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], inner).into_response()
}

/// One row of a selection tree, regardless of which flow renders
/// it. `key` lands in data-idx: numeric indices for import, snippet
/// ids for export - the consuming form declares which via
/// data-keys.
struct TreeItem {
    key: String,
    title: String,
    folder: String,
    checked: bool,
    badge: bool,
}

/// Nested folder node used to assemble the selection tree.
#[derive(Default)]
struct TreeNode<'a> {
    children: std::collections::BTreeMap<String, TreeNode<'a>>,
    items: Vec<&'a TreeItem>,
}

fn render_selection_tree(items: &[TreeItem]) -> String {
    let mut root = TreeNode::default();
    for item in items {
        let folder = item.folder.trim();
        let node = if folder.is_empty() {
            root.children.entry("(no folder)".to_string()).or_default()
        } else {
            folder
                .split('/')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .fold(&mut root, |n, seg| {
                    n.children.entry(seg.to_string()).or_default()
                })
        };
        node.items.push(item);
    }
    let mut out = String::new();
    render_tree_children(&root, &mut out);
    out
}

fn tree_node_size(node: &TreeNode) -> usize {
    node.items.len() + node.children.values().map(tree_node_size).sum::<usize>()
}

/// True when every entry under this folder (recursively) carries the
/// duplicate badge - the folder row then gets its own badge so the
/// admin can skip expanding it. Export trees never badge, so this
/// stays false there.
fn tree_node_all_badged(node: &TreeNode) -> bool {
    tree_node_size(node) > 0
        && node.items.iter().all(|i| i.badge)
        && node.children.values().all(tree_node_all_badged)
}

fn render_tree_children(node: &TreeNode, out: &mut String) {
    for (name, child) in &node.children {
        out.push_str(&format!(
            "<div class=\"imp-folder\">\
               <div class=\"imp-folder-row\">\
                 <button type=\"button\" class=\"imp-toggle\" aria-label=\"expand\">&#9656;</button>\
                 <label><input type=\"checkbox\" class=\"imp-folder-cb\" /> \
                   <strong>&#128193; {name}</strong> <span class=\"muted\">({count})</span>{badge}</label>\
               </div>\
               <div class=\"imp-children\" hidden>",
            name = escape_html(name),
            count = tree_node_size(child),
            badge = if tree_node_all_badged(child) {
                " <span class=\"imp-badge\">all duplicates</span>"
            } else {
                ""
            },
        ));
        render_tree_children(child, out);
        out.push_str("</div></div>");
    }
    for item in &node.items {
        out.push_str(&format!(
            "<div class=\"imp-item\" data-title=\"{title_lc}\">\
               <label><input type=\"checkbox\" class=\"imp-item-cb\" data-idx=\"{key}\"{checked} /> \
                 {title}{badge}</label>\
             </div>",
            key = escape_html(&item.key),
            title = escape_html(&item.title),
            title_lc = escape_html(&item.title.to_lowercase()),
            checked = if item.checked { " checked" } else { "" },
            badge = if item.badge {
                " <span class=\"imp-badge\">duplicate</span>"
            } else {
                ""
            },
        ));
    }
}

#[derive(Debug, Deserialize)]
pub struct ImportConfirmForm {
    /// The full normalized entry list, JSON (round-tripped from the
    /// preview step's hidden field).
    pub payload: String,
    /// JSON array of selected indices into `payload`.
    pub selected: String,
}

/// POST /dashboard/library/import - insert the selected entries via
/// the same create path the dashboard form uses (versioning, audit
/// per snippet), then redirect back to the library with a banner.
pub async fn library_import_confirm(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Form(form): Form<ImportConfirmForm>,
) -> Response {
    let entries: Vec<ImportFileEntry> = match serde_json::from_str(&form.payload) {
        Ok(e) => e,
        Err(_) => return Redirect::to("/dashboard/library/import").into_response(),
    };
    let selected: Vec<usize> = serde_json::from_str(&form.selected).unwrap_or_default();

    // Dedupe against current library titles AND within the batch,
    // trimmed lowercase title - the desktop importer's rule.
    let mut existing: std::collections::HashSet<String> = load_library(&state)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.title.trim().to_lowercase())
        .collect();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for idx in selected {
        let Some(entry) = entries.get(idx) else {
            continue;
        };
        let key = entry.title.trim().to_lowercase();
        if key.is_empty() || existing.contains(&key) {
            skipped += 1;
            continue;
        }
        let auth = crate::auth::AuthUser(admin.claims.clone());
        let res = crate::handlers::library::create(
            State(state.clone()),
            auth,
            Json(crate::handlers::library::CreateBody {
                id: Uuid::new_v4().to_string(),
                payload: crate::handlers::library::LibraryPayload {
                    title: entry.title.trim().to_string(),
                    body: entry.body.clone(),
                    tags: entry.tags.clone(),
                    folder_path: entry
                        .folder_path
                        .clone()
                        .map(|f| f.trim().to_string())
                        .filter(|f| !f.is_empty()),
                },
            }),
        )
        .await;
        match res {
            Ok(_) => {
                existing.insert(key);
                imported += 1;
            }
            Err(_) => skipped += 1,
        }
    }

    let actor_email = crate::audit::lookup_actor_email(&state.pool, admin.user_id()).await;
    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(admin.user_id()),
            actor_email: &actor_email,
            action: crate::audit::action::LIBRARY_IMPORT,
            target_kind: Some("library"),
            target_id: None,
            details: Some(serde_json::json!({
                "imported": imported,
                "skipped": skipped,
            })),
        },
    )
    .await;

    Redirect::to(&format!(
        "/dashboard/library?imported={imported}&skipped={skipped}"
    ))
    .into_response()
}

fn render_library_cards_inner(rows: &[&LibraryRow]) -> String {
    if rows.is_empty() {
        return String::from(
            "<p class=\"muted\">No library snippets in this view. \
             Add one above or pick a different folder.</p>",
        );
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&render_library_card(r));
    }
    out
}

/// The "Add to library" form at the top of the main area. Pre-fills
/// the folder field with the currently-selected folder so adding a
/// snippet while you're in a folder defaults to that folder.
fn library_create_form(selected: &str) -> String {
    let prefilled_folder = match selected {
        FOLDER_ALL | FOLDER_UNFILED => String::new(),
        other => other.to_string(),
    };
    format!(
        "<form class=\"lib-form stack\" \
              hx-post=\"/dashboard/library\" \
              hx-target=\"#library-list\" \
              hx-swap=\"afterbegin\" \
              hx-on::after-request=\"if(event.detail.successful) this.reset()\">\
           <div class=\"row\">\
             <label>Title<input type=\"text\" name=\"title\" required /></label>\
             <label>Folder<input type=\"text\" name=\"folder_path\" \
                placeholder=\"e.g. Replies/Billing\" value=\"{prefill}\" /></label>\
           </div>\
           <div class=\"field-block\" role=\"group\" aria-label=\"Body\">\
             <span>Body</span>\
             <div class=\"format-toolbar\" data-target=\"lib-create-body\">{toolbar}</div>\
             <textarea id=\"lib-create-body\" name=\"body\" required></textarea>\
           </div>\
           <label>Tags (comma-separated)\
             <input type=\"text\" name=\"tags\" placeholder=\"billing, refund\" /></label>\
           <div class=\"actions\"><button class=\"primary\" type=\"submit\">Add to library</button></div>\
         </form>",
        prefill = escape_html(&prefilled_folder),
        toolbar = library_format_toolbar(),
    )
}

/// Buttons that wrap the textarea selection with markdown markers.
/// Wired up by LIBRARY_PAGE_JS which finds the toolbar's
/// data-target sibling textarea.
fn library_format_toolbar() -> &'static str {
    "<button type=\"button\" class=\"fmt-btn\" data-prefix=\"**\" data-suffix=\"**\" title=\"Bold\"><b>B</b></button>\
     <button type=\"button\" class=\"fmt-btn\" data-prefix=\"*\" data-suffix=\"*\" title=\"Italic\"><i>I</i></button>\
     <button type=\"button\" class=\"fmt-btn\" data-prefix=\"`\" data-suffix=\"`\" title=\"Inline code\"><code>{}</code></button>\
     <button type=\"button\" class=\"fmt-btn\" data-prefix=\"[\" data-suffix=\"](https://)\" title=\"Link\">link</button>"
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
        Some(f) if !f.is_empty() => format!(" | <span class=\"muted\">{}</span>", escape_html(f)),
        _ => String::new(),
    };
    // Usage pill: only shown for snippets that have actually been
    // pasted. A "0 uses" pill on every brand-new snippet would be
    // noisy and undermine the signal of the metric.
    let usage_pill = if r.use_count > 0 {
        let when = r
            .last_used
            .map(|t| format!(", last {when}", when = format_relative(t)))
            .unwrap_or_default();
        format!(
            " <span class=\"pill usage\" title=\"team-wide paste count\">used {count}{when}</span>",
            count = format_thousands(r.use_count),
        )
    } else {
        String::new()
    };
    // Version (`r.version`) is the optimistic-concurrency counter the
    // wire protocol uses to reject stale PUTs. It's plumbed into the
    // hidden expected_version form input on the edit form (the
    // mechanism that actually matters), so the admin-facing card
    // doesn't need to display it - it was leaking an internal
    // mechanic into the operator's reading task.
    format!(
        "<div class=\"library-card\" id=\"lib-{id_attr}\" \
             draggable=\"true\" data-snippet-id=\"{id_attr}\">\
           <div class=\"card-head\">\
             <span class=\"title\">{title}</span>{folder}{tags}{usage_pill}\
             <span class=\"meta\">updated {when}</span>\
           </div>\
           <pre class=\"body\">{body}</pre>\
           <div class=\"card-actions\">\
             <button class=\"btn\" \
                hx-get=\"/dashboard/library/{id_attr}/edit\" \
                hx-target=\"closest .library-card\" hx-swap=\"outerHTML\">Edit</button>\
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
        when = format_relative(r.updated_at),
        tags = tags_html,
        folder = folder,
        usage_pill = usage_pill,
    )
}

/// The inline edit form, rendered into the slot where a library card
/// used to be. Same shape as the create form but with the existing
/// row's values pre-filled and a hidden expected_version for
/// optimistic-concurrency parity with the JSON PUT handler.
fn render_library_edit_form(r: &LibraryRow) -> String {
    format!(
        "<form class=\"library-card lib-edit-form stack\" id=\"lib-{id_attr}\" \
              hx-put=\"/dashboard/library/{id_attr}\" \
              hx-target=\"closest .library-card\" hx-swap=\"outerHTML\">\
           <input type=\"hidden\" name=\"expected_version\" value=\"{ver}\" />\
           <div class=\"row\">\
             <label>Title<input type=\"text\" name=\"title\" value=\"{title_attr}\" required /></label>\
             <label>Folder<input type=\"text\" name=\"folder_path\" \
                placeholder=\"e.g. Replies/Billing\" value=\"{folder_attr}\" /></label>\
           </div>\
           <div class=\"field-block\" role=\"group\" aria-label=\"Body\">\
             <span>Body</span>\
             <div class=\"format-toolbar\" data-target=\"lib-edit-body-{id_attr}\">{toolbar}</div>\
             <textarea id=\"lib-edit-body-{id_attr}\" name=\"body\" required>{body_text}</textarea>\
           </div>\
           <label>Tags (comma-separated)\
             <input type=\"text\" name=\"tags\" value=\"{tags_attr}\" placeholder=\"billing, refund\" /></label>\
           <div class=\"actions\">\
             <button type=\"button\" class=\"btn\" \
                hx-get=\"/dashboard/library/{id_attr}/card\" \
                hx-target=\"closest .library-card\" hx-swap=\"outerHTML\">Cancel</button>\
             <button class=\"primary\" type=\"submit\">Save changes</button>\
           </div>\
         </form>",
        id_attr = escape_html(&r.id),
        title_attr = escape_html(&r.title),
        folder_attr = escape_html(r.folder_path.as_deref().unwrap_or("")),
        // Reuse escape_html for textarea content - same set works for
        // body context and textarea content (textarea is special only
        // for `</textarea>` which our escape catches via <).
        body_text = escape_html(&r.body),
        tags_attr = escape_html(&decode_tags_for_form(&r.tags)),
        ver = r.version,
        toolbar = library_format_toolbar(),
    )
}

/// `,billing,refund,` -> `billing, refund`. The DB format is bracket-
/// delimited for cheap LIKE matching; the form value is the human
/// shape.
fn decode_tags_for_form(stored: &str) -> String {
    stored
        .split(',')
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Load explicit folder rows for the sidebar. Empty Vec is fine
/// (and the load_or_default flow downstream treats it as such);
/// the tree renderer just doesn't get any extra paths to union.
async fn load_library_folders(state: &AppState) -> Vec<FolderRow> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT path, sort_order FROM library_folders ORDER BY sort_order, path",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
}

async fn load_library(state: &AppState) -> Result<Vec<LibraryRow>, ()> {
    // LEFT JOIN to library_usage so we can show per-snippet paste
    // counts on each card. Aggregated across all users -
    // breaking it down per-user would explode the row count and
    // isn't useful at the team-library list level. The query is
    // O(n_snippets) with a covering index on (snippet_id) - cheap
    // even at thousands of snippets and millions of usage rows.
    sqlx::query_as::<_, LibraryRow>(
        "SELECT s.id, s.title, s.body, s.tags, s.folder_path, s.version, s.updated_at, \
                COALESCE(SUM(u.usage_count), 0) AS use_count, \
                MAX(u.last_used) AS last_used \
         FROM library_snippets s \
         LEFT JOIN library_usage u ON u.snippet_id = s.id \
         WHERE s.is_deleted = 0 \
         GROUP BY s.id \
         ORDER BY s.updated_at DESC",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| ())
}

#[derive(Debug, Deserialize)]
pub struct LibraryPageQuery {
    #[serde(default)]
    pub folder: Option<String>,
    /// Free-text filter over title/body/tags. Empty or absent means
    /// no filter. Joins the folder filter (AND semantics).
    #[serde(default)]
    pub q: Option<String>,
    /// Set by the import-confirm redirect so the page can show a
    /// one-shot result banner.
    #[serde(default)]
    pub imported: Option<usize>,
    #[serde(default)]
    pub skipped: Option<usize>,
}

/// Apply the search-bar filter on top of the folder filter.
/// Case-insensitive substring match over title, body, and the raw
/// tags string. In-memory for the same reason as
/// `filter_library_rows`: the rows are already loaded for the
/// sidebar counts.
fn filter_library_query<'a>(rows: Vec<&'a LibraryRow>, q: &Option<String>) -> Vec<&'a LibraryRow> {
    let needle = q.as_deref().unwrap_or("").trim().to_lowercase();
    if needle.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| {
            r.title.to_lowercase().contains(&needle)
                || r.body.to_lowercase().contains(&needle)
                || r.tags.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Inline drag-drop + formatting-toolbar wiring for the library page.
/// Scoped via `data-*` attributes on the library DOM so a stray
/// global keypress can't trigger formatting on an unrelated input.
const LIBRARY_PAGE_JS: &str = r##"<script>
(function () {
  // ---- Selection-tree modal (shared by export picker + import
  // preview). Fragments are inserted into #library-modal-body, so
  // every behaviour below is delegated - inserted markup carries no
  // scripts of its own. ----
  var modal = document.getElementById("library-modal");
  var modalBody = document.getElementById("library-modal-body");

  function openModal(html) {
    modalBody.innerHTML = html;
    modal.hidden = false;
    updateTreeCounts();
  }
  function closeModal() {
    modal.hidden = true;
    modalBody.innerHTML = "";
  }
  document.getElementById("library-modal-close").addEventListener("click", closeModal);
  modal.addEventListener("click", function (e) {
    if (e.target === modal) closeModal();
  });

  function treeItems() {
    return Array.prototype.slice.call(modalBody.querySelectorAll(".imp-item-cb"));
  }
  function updateTreeCounts() {
    var countEl = modalBody.querySelector("#imp-count");
    if (!countEl) return;
    var cbs = treeItems();
    var sel = cbs.filter(function (c) { return c.checked; }).length;
    countEl.textContent = sel + " of " + cbs.length + " selected";
    var confirm = modalBody.querySelector(".imp-confirm");
    if (confirm) confirm.disabled = sel === 0;
    var master = modalBody.querySelector("#imp-master");
    if (master) {
      master.checked = sel === cbs.length && cbs.length > 0;
      master.indeterminate = sel > 0 && sel < cbs.length;
    }
    Array.prototype.slice.call(modalBody.querySelectorAll(".imp-folder")).reverse().forEach(function (f) {
      var inner = f.querySelectorAll(".imp-item-cb");
      var innerSel = 0;
      inner.forEach(function (c) { if (c.checked) innerSel++; });
      var fcb = f.querySelector(":scope > .imp-folder-row .imp-folder-cb");
      if (!fcb) return;
      fcb.checked = inner.length > 0 && innerSel === inner.length;
      fcb.indeterminate = innerSel > 0 && innerSel < inner.length;
    });
  }
  modalBody.addEventListener("click", function (e) {
    var btn = e.target.closest(".imp-toggle");
    if (!btn) return;
    var children = btn.closest(".imp-folder").querySelector(":scope > .imp-children");
    var open = children.hasAttribute("hidden");
    if (open) children.removeAttribute("hidden"); else children.setAttribute("hidden", "");
    btn.innerHTML = open ? "&#9662;" : "&#9656;";
  });
  modalBody.addEventListener("change", function (e) {
    if (e.target.id === "imp-master") {
      var on = e.target.checked;
      treeItems().forEach(function (c) { c.checked = on; });
    } else if (e.target.classList.contains("imp-folder-cb")) {
      e.target.closest(".imp-folder").querySelectorAll(".imp-item-cb").forEach(function (c) {
        c.checked = e.target.checked;
      });
    }
    updateTreeCounts();
  });
  modalBody.addEventListener("input", function (e) {
    if (e.target.id !== "imp-search") return;
    var q = e.target.value.trim().toLowerCase();
    modalBody.querySelectorAll(".imp-item").forEach(function (it) {
      it.style.display = !q || it.dataset.title.indexOf(q) !== -1 ? "" : "none";
    });
    Array.prototype.slice.call(modalBody.querySelectorAll(".imp-folder")).reverse().forEach(function (f) {
      var anyVisible = Array.prototype.some.call(
        f.querySelectorAll(".imp-item"),
        function (it) { return it.style.display !== "none"; }
      );
      f.style.display = anyVisible ? "" : "none";
      var children = f.querySelector(":scope > .imp-children");
      var toggle = f.querySelector(":scope > .imp-folder-row .imp-toggle");
      if (q && anyVisible) {
        children.removeAttribute("hidden");
        if (toggle) toggle.innerHTML = "&#9662;";
      } else if (!q) {
        children.setAttribute("hidden", "");
        if (toggle) toggle.innerHTML = "&#9656;";
      }
    });
  });
  // On submit, write the checked keys into the form's hidden
  // `selected` field. Import keys are numeric indices; export keys
  // are snippet ids - the server side knows which it expects.
  modalBody.addEventListener("submit", function (e) {
    var form = e.target.closest("form");
    if (!form || !form.classList.contains("imp-form")) return;
    var sel = treeItems()
      .filter(function (c) { return c.checked; })
      .map(function (c) { return c.dataset.idx; });
    var field = form.querySelector("input[name=selected]");
    if (field) {
      field.value = JSON.stringify(
        form.dataset.keys === "numeric" ? sel.map(Number) : sel
      );
    }
    // Export downloads an attachment and the page stays; close the
    // modal so the admin isn't left staring at a stale picker.
    if (form.dataset.closeOnSubmit === "1") setTimeout(closeModal, 250);
  });

  // ---- Toolbar: Export opens the picker fragment; Import pops the
  // file browser and posts the file's text to the preview endpoint ----
  document.getElementById("library-export-btn").addEventListener("click", function () {
    fetch("/dashboard/library/export/picker")
      .then(function (r) { return r.text(); })
      .then(openModal);
  });
  var importFile = document.getElementById("library-import-file");
  document.getElementById("library-import-btn").addEventListener("click", function () {
    importFile.value = "";
    importFile.click();
  });
  importFile.addEventListener("change", function () {
    var f = importFile.files && importFile.files[0];
    if (!f) return;
    var reader = new FileReader();
    reader.onload = function () {
      fetch("/dashboard/library/import/preview", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: "content=" + encodeURIComponent(reader.result),
      })
        .then(function (r) { return r.text(); })
        .then(openModal);
    };
    reader.readAsText(f);
  });
  // ---- Format toolbar: wraps the textarea selection with markdown markers ----
  document.body.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest(".fmt-btn");
    if (!btn) return;
    e.preventDefault();
    var toolbar = btn.closest(".format-toolbar");
    if (!toolbar) return;
    var targetId = toolbar.getAttribute("data-target");
    var ta = targetId && document.getElementById(targetId);
    if (!ta) return;
    var prefix = btn.getAttribute("data-prefix") || "";
    var suffix = btn.getAttribute("data-suffix") || "";
    var start = ta.selectionStart, end = ta.selectionEnd;
    var sel = ta.value.slice(start, end);
    var before = ta.value.slice(0, start);
    var after = ta.value.slice(end);
    ta.value = before + prefix + sel + suffix + after;
    // Land caret between markers when nothing was selected; otherwise
    // re-select the wrapped content so further formatting layers cleanly.
    if (sel.length === 0) {
      var p = start + prefix.length;
      ta.setSelectionRange(p, p);
    } else {
      ta.setSelectionRange(start + prefix.length, start + prefix.length + sel.length);
    }
    ta.focus();
  });

  // ---- Drag-drop: snippets into folders, AND folders onto folders ----
  //
  // Two independent drag types coexist on this page:
  //   1. A library-card being dragged onto a folder node = snippet move.
  //   2. A folder node being dragged onto another folder node OR the
  //      root drop zone = folder rename / nest / unnest.
  //
  // We disambiguate at dragstart by inspecting which element the
  // browser handed us. `draggingKind` is "snippet" or "folder";
  // `draggingId` is the snippet id or source folder path.
  var draggingKind = null;
  var draggingId = null;

  function clearDropHighlights() {
    document.querySelectorAll(
      ".lib-folder-row.drop-target, .lib-folder-root-drop.drop-target"
    ).forEach(function (n) { n.classList.remove("drop-target"); });
  }

  document.body.addEventListener("dragstart", function (e) {
    var folderSrc = e.target.closest &&
      e.target.closest(".lib-folder-row[data-folder-source]");
    if (folderSrc) {
      draggingKind = "folder";
      draggingId = folderSrc.getAttribute("data-folder-path") || "";
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", "folder:" + draggingId);
      folderSrc.classList.add("dragging");
      return;
    }
    var card = e.target.closest && e.target.closest(".library-card[data-snippet-id]");
    if (card) {
      draggingKind = "snippet";
      draggingId = card.getAttribute("data-snippet-id");
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", draggingId);
      card.classList.add("dragging");
    }
  });
  document.body.addEventListener("dragend", function (e) {
    var dragged = e.target.closest &&
      e.target.closest(".library-card, .lib-folder-row");
    if (dragged) dragged.classList.remove("dragging");
    draggingKind = null;
    draggingId = null;
    clearDropHighlights();
    clearDropPositionClasses();
  });
  document.body.addEventListener("dragover", function (e) {
    var rootDrop = e.target.closest && e.target.closest(".lib-folder-root-drop");
    // Root drop only accepts folder drags. Snippets dropped on the
    // root zone would mean "unfile" - but Unfiled already serves
    // that purpose, so we don't double up.
    if (rootDrop && draggingKind === "folder") {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      rootDrop.classList.add("drop-target");
      return;
    }
    var node = e.target.closest && e.target.closest(".lib-folder-row[data-droppable]");
    if (!node) return;
    // A folder can't be dropped on itself or on its own descendants.
    if (draggingKind === "folder") {
      var target = node.getAttribute("data-folder-path") || "";
      if (target === draggingId || target.indexOf(draggingId + "/") === 0) {
        return;
      }
    }
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    // Decide nest vs reorder visual based on cursor Y. Snippet drags
    // and alpha-mode folder drags always fall through to "into"
    // (existing nest highlight). Manual-mode folder drags pick up
    // an above/below indicator from classifyFolderDrop.
    clearDropPositionClasses();
    var position = (draggingKind === "folder")
      ? classifyFolderDrop(node, e.clientY)
      : "into";
    if (position === "above") {
      node.classList.add("drop-above");
      node.classList.remove("drop-target");
    } else if (position === "below") {
      node.classList.add("drop-below");
      node.classList.remove("drop-target");
    } else {
      node.classList.add("drop-target");
    }
  });
  document.body.addEventListener("dragleave", function (e) {
    var node = e.target.closest && e.target.closest(
      ".lib-folder-row, .lib-folder-root-drop"
    );
    if (node) node.classList.remove("drop-target", "drop-above", "drop-below");
  });
  document.body.addEventListener("drop", function (e) {
    var rootDrop = e.target.closest && e.target.closest(".lib-folder-root-drop");
    if (rootDrop && draggingKind === "folder" && draggingId) {
      e.preventDefault();
      clearDropHighlights();
      clearDropPositionClasses();
      var leaf0 = draggingId.split("/").pop();
      submitFolderMove(draggingId, leaf0 || "");
      return;
    }
    var node = e.target.closest && e.target.closest(".lib-folder-row[data-droppable]");
    if (!node) return;
    e.preventDefault();
    var target = node.getAttribute("data-folder-path") || "";
    // Capture position BEFORE clearing classes so we know whether
    // this was a nest or a reorder. classifyFolderDrop reads the
    // cursor freshly; classes were just for visual feedback.
    var position = (draggingKind === "folder")
      ? classifyFolderDrop(node, e.clientY)
      : "into";
    clearDropHighlights();
    clearDropPositionClasses();
    if (draggingKind === "snippet" && draggingId) {
      var folder = node.hasAttribute("data-unfiled") ? "" : target;
      var params = new URLSearchParams();
      params.append("folder_path", folder);
      fetch("/dashboard/library/" + encodeURIComponent(draggingId) + "/move", {
        method: "PUT",
        body: params,
      }).then(function (r) {
        if (!r.ok) { console.warn("snippet move failed", r.status); return; }
        refreshLibrary();
      });
    } else if (draggingKind === "folder" && draggingId) {
      if (!target || target === draggingId || target.indexOf(draggingId + "/") === 0) {
        return;
      }
      if (node.hasAttribute("data-unfiled")) return;
      if (position === "above" || position === "below") {
        submitFolderReorder(target, position);
      } else {
        var leaf = draggingId.split("/").pop();
        submitFolderMove(draggingId, target + "/" + leaf);
      }
    }
  });

  function submitFolderMove(oldPath, newPath) {
    var params = new URLSearchParams();
    params.append("old", oldPath);
    params.append("new", newPath);
    fetch("/dashboard/library/folders/move", {
      method: "POST",
      body: params,
    }).then(function (r) {
      if (!r.ok) {
        r.text().then(function (msg) {
          console.warn("folder move failed", r.status, msg);
        });
        return;
      }
      refreshLibrary();
    });
  }

  function refreshLibrary() {
    if (!window.htmx) return;
    var list = document.getElementById("library-list");
    var sidebar = document.getElementById("library-sidebar");
    if (list) window.htmx.trigger(list, "refresh-now");
    if (sidebar) window.htmx.trigger(sidebar, "refresh-now");
  }

  // ---- Folder collapse / expand ----
  //
  // The set of currently-collapsed folder paths lives in
  // localStorage so the choice survives page reloads + the 10s
  // sidebar poll. Children of a collapsed folder are hidden via
  // CSS (.lib-folder-collapsed-child); the caret on the parent
  // flips between right-pointing (collapsed) and down-pointing
  // (open) glyphs.
  var FOLDER_COLLAPSE_KEY = "snipdesk-library-collapsed-folders";
  function loadCollapsed() {
    try { return JSON.parse(localStorage.getItem(FOLDER_COLLAPSE_KEY) || "[]"); }
    catch (_e) { return []; }
  }
  function saveCollapsed(arr) {
    try { localStorage.setItem(FOLDER_COLLAPSE_KEY, JSON.stringify(arr)); }
    catch (_e) {}
  }
  function applyFolderCollapse() {
    var collapsed = loadCollapsed();
    document.querySelectorAll(
      ".lib-folder-row[data-folder-path]"
    ).forEach(function (n) {
      var p = n.getAttribute("data-folder-path") || "";
      // Hide if any ancestor path is in the collapsed set. Use
      // prefix-with-slash so "Replies" doesn't hide a sibling
      // path that happens to start with "Replies".
      var hide = collapsed.some(function (parent) {
        return p.indexOf(parent + "/") === 0;
      });
      n.classList.toggle("lib-folder-collapsed-child", hide);
    });
    document.querySelectorAll(
      ".lib-folder-caret[data-folder-caret]"
    ).forEach(function (k) {
      var p = k.getAttribute("data-folder-caret") || "";
      var isCollapsed = collapsed.indexOf(p) !== -1;
      k.classList.toggle("collapsed", isCollapsed);
      // ▸ = right-pointing small triangle (collapsed);
      // ▾ = down-pointing small triangle (expanded).
      k.textContent = isCollapsed ? "\u25B8" : "\u25BE";
    });
  }
  function toggleFolderCollapsed(path) {
    if (!path) return;
    var c = loadCollapsed();
    var i = c.indexOf(path);
    if (i === -1) c.push(path); else c.splice(i, 1);
    saveCollapsed(c);
  }

  // Folder-row click navigation. The row no longer wraps an
  // inner <a>, so we navigate explicitly. Caret clicks are
  // handled first (with stopPropagation) so toggling collapse
  // doesn't double-fire as navigation. Any other click on the
  // row resolves to its data-folder-href and goes there.
  document.body.addEventListener("click", function (e) {
    var caret = e.target.closest && e.target.closest(".lib-folder-caret");
    if (caret) {
      e.preventDefault();
      e.stopPropagation();
      toggleFolderCollapsed(caret.getAttribute("data-folder-caret") || "");
      applyFolderCollapse();
      return;
    }
    var row = e.target.closest && e.target.closest(".lib-folder-row[data-folder-href]");
    if (row) {
      // Ignore clicks during a drag (browser fires a phantom
      // click on drop-end in some engines).
      if (e.defaultPrevented) return;
      var href = row.getAttribute("data-folder-href");
      if (href) window.location.href = href;
    }
  });
  // Keyboard parity for the focused caret. Space + Enter behave
  // like a click; matches the role="button" semantics we set on
  // the caret span.
  document.body.addEventListener("keydown", function (e) {
    if (e.key !== " " && e.key !== "Enter") return;
    var caret = e.target.closest && e.target.closest(".lib-folder-caret");
    if (!caret) return;
    e.preventDefault();
    e.stopPropagation();
    toggleFolderCollapsed(caret.getAttribute("data-folder-caret") || "");
    applyFolderCollapse();
  });

  applyFolderCollapse();

  // ---- Sort mode (alphabetical | manual) ----
  //
  // The server always emits siblings in alphabetical order (the
  // BTreeSet walk is alphabetical). In manual mode the JS pass
  // re-shuffles the DOM so siblings appear in (data-sort-order,
  // path) order instead. Choice persists in localStorage so the
  // 10s sidebar refresh keeps respecting it.
  var SORT_MODE_KEY = "snipdesk-library-sort-mode";
  function loadSortMode() {
    try { return localStorage.getItem(SORT_MODE_KEY) || "alpha"; }
    catch (_e) { return "alpha"; }
  }
  function saveSortMode(m) {
    try { localStorage.setItem(SORT_MODE_KEY, m); } catch (_e) {}
  }

  function parentOf(path) {
    var i = path.lastIndexOf("/");
    return i === -1 ? "" : path.substring(0, i);
  }

  function applySortMode() {
    var sel = document.getElementById("lib-sort-mode-select");
    var mode = loadSortMode();
    if (sel) sel.value = mode;
    // Re-shuffle in BOTH modes. Alphabetical sorts by path; manual
    // sorts by (sort_order, path). The previous implementation
    // returned early in alpha mode, so switching manual -> alpha
    // left the DOM in manual order until the next sidebar fetch -
    // hence the "switching sort mode requires a refresh" bug.
    var sidebar = document.getElementById("library-sidebar");
    if (!sidebar) return;
    var all = Array.from(sidebar.querySelectorAll(
      ".lib-folder-row[data-folder-source]"
    ));
    if (all.length === 0) return;
    var byParent = {};
    all.forEach(function (n) {
      var p = n.getAttribute("data-folder-path") || "";
      var par = parentOf(p);
      if (!byParent[par]) byParent[par] = [];
      byParent[par].push(n);
    });
    Object.keys(byParent).forEach(function (par) {
      byParent[par].sort(function (a, b) {
        var aP = a.getAttribute("data-folder-path") || "";
        var bP = b.getAttribute("data-folder-path") || "";
        if (mode === "manual") {
          var aO = parseInt(a.getAttribute("data-sort-order") || "0", 10);
          var bO = parseInt(b.getAttribute("data-sort-order") || "0", 10);
          if (aO !== bO) return aO - bO;
        }
        return aP.localeCompare(bP);
      });
    });
    // Anchor for re-inserts: the root drop zone (or first
    // pseudo-node) marks the boundary between specials and real
    // folders. We append the rebuilt tree right after it so the
    // pseudo-nodes (All, Unfiled, drop zone) keep their leading
    // position.
    var anchor = sidebar.querySelector(".lib-folder-root-drop") ||
                 sidebar.querySelector(".lib-folder-row");
    if (!anchor) return;
    function emitChildrenOf(par) {
      var kids = byParent[par] || [];
      kids.forEach(function (n) {
        sidebar.insertBefore(n, anchor.nextSibling);
        anchor = n;
        emitChildrenOf(n.getAttribute("data-folder-path"));
      });
    }
    emitChildrenOf("");
  }

  var sortSel = document.getElementById("lib-sort-mode-select");
  if (sortSel) {
    sortSel.value = loadSortMode();
    sortSel.addEventListener("change", function () {
      saveSortMode(sortSel.value);
      applySortMode();
      applyFolderCollapse();
    });
  }
  applySortMode();

  // ---- Folder reorder via in-row drop indicators ----
  //
  // Previous revisions injected separate "insert zone" elements
  // between sibling folder rows on dragstart. Chromium aborts a
  // drag when the dragstart handler mutates the surrounding DOM,
  // which is exactly what showInsertZones did - that's why only
  // the very first draggable row's drag survived, and why nested
  // folders with single-sibling groups (siblings.length < 2 in
  // the old guard) coincidentally worked: no zones got inserted,
  // no DOM mutation, drag survived.
  //
  // The new mechanism does ZERO DOM mutation during dragstart.
  // dragover on a folder row computes the cursor's Y position
  // relative to the row and adds one of three classes:
  //   - drop-above (top 30% of the row)  -> insert dragged BEFORE this row
  //   - drop-below (bottom 30%)          -> insert dragged AFTER this row
  //   - drop-target (middle 40%)         -> nest dragged INTO this row
  // Reorder is only available in manual sort mode + when dragged
  // and target share a parent. Cross-parent or alpha-mode drops
  // fall through to the existing nest behaviour.
  function siblingsOf(path) {
    var par = parentOf(path);
    var sidebar = document.getElementById("library-sidebar");
    if (!sidebar) return [];
    return Array.from(sidebar.querySelectorAll(
      ".lib-folder-row[data-folder-source]"
    )).filter(function (n) {
      return parentOf(n.getAttribute("data-folder-path") || "") === par;
    });
  }
  function clearDropPositionClasses() {
    document.querySelectorAll(
      ".lib-folder-row.drop-above, .lib-folder-row.drop-below"
    ).forEach(function (n) {
      n.classList.remove("drop-above", "drop-below");
    });
  }
  function classifyFolderDrop(node, clientY) {
    // Only attempt reorder in manual mode + same-parent target.
    // Anything else collapses to nest (drop-target).
    if (loadSortMode() !== "manual") return "into";
    if (!node.hasAttribute("data-folder-source")) return "into";
    var targetPath = node.getAttribute("data-folder-path") || "";
    if (parentOf(targetPath) !== parentOf(draggingId || "")) return "into";
    var rect = node.getBoundingClientRect();
    var ratio = (clientY - rect.top) / Math.max(rect.height, 1);
    if (ratio < 0.30) return "above";
    if (ratio > 0.70) return "below";
    return "into";
  }
  function submitFolderReorder(targetPath, position) {
    var siblings = siblingsOf(draggingId).map(function (n) {
      return n.getAttribute("data-folder-path") || "";
    });
    var reordered = siblings.filter(function (p) { return p !== draggingId; });
    var idx = reordered.indexOf(targetPath);
    if (idx === -1) {
      reordered.push(draggingId);
    } else if (position === "above") {
      reordered.splice(idx, 0, draggingId);
    } else {
      reordered.splice(idx + 1, 0, draggingId);
    }
    fetch("/dashboard/library/folders/reorder", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        parent: parentOf(draggingId),
        paths: reordered,
      }),
    }).then(function (r) {
      if (!r.ok) {
        r.text().then(function (msg) {
          console.warn("folder reorder failed", r.status, msg);
        });
        return;
      }
      refreshLibrary();
    });
  }

  // ---- "+ New folder" form ----
  var createForm = document.getElementById("lib-folder-create-form");
  if (createForm) {
    createForm.addEventListener("submit", function (e) {
      e.preventDefault();
      var input = document.getElementById("lib-folder-create-input");
      if (!input) return;
      var path = input.value.trim();
      if (!path) return;
      var params = new URLSearchParams();
      params.append("path", path);
      fetch("/dashboard/library/folders/create", {
        method: "POST",
        body: params,
      }).then(function (r) {
        if (!r.ok) {
          r.text().then(function (msg) {
            console.warn("folder create failed", r.status, msg);
            alert("Couldn't create folder: " + msg);
          });
          return;
        }
        input.value = "";
        refreshLibrary();
      });
    });
  }

  // The sidebar polls every 10s and may swap in a fresh tree; the
  // sidebar fragment also re-renders after libraryChanged events
  // from create/update/delete. Either way, reapply collapse +
  // sort state once htmx has finished the swap.
  document.body.addEventListener("htmx:afterSwap", function (e) {
    if (e.target && e.target.id === "library-sidebar") {
      applySortMode();
      applyFolderCollapse();
    }
  });

  // The hx-trigger attributes on the sidebar + cards list now
  // include "refresh-now" baked in by the Rust template - no JS
  // mutation needed. Previous attempts to splice it in here ran
  // AFTER htmx had already processed the attribute, so triggers
  // fired from JS were silently dropped and mutations only
  // surfaced on the next 5s/10s tick.
})();
</script>"##;

#[derive(sqlx::FromRow)]
struct LibraryRow {
    id: String,
    title: String,
    body: String,
    tags: String,
    folder_path: Option<String>,
    version: i64,
    updated_at: i64,
    /// Team-wide paste count for this snippet (SUM over library_usage).
    /// Defaults to 0 when no users have pasted it; the LEFT JOIN +
    /// COALESCE in load_library() makes this column always present.
    #[sqlx(default)]
    use_count: i64,
    /// Most-recent paste timestamp across the team (unix seconds);
    /// None when nobody has pasted yet.
    #[sqlx(default)]
    last_used: Option<i64>,
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
            // HX-Trigger fires libraryChanged so the folder sidebar
            // and any other listeners refresh immediately rather
            // than waiting for the next 10s poll.
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (
                    header::HeaderName::from_static("hx-trigger"),
                    "libraryChanged",
                ),
            ],
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
                // A brand-new card has no usage yet - the next page
                // refresh will pick up real numbers from library_usage.
                use_count: 0,
                last_used: None,
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

#[derive(Debug, Deserialize)]
pub struct LibraryEditForm {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub folder_path: String,
    /// Optimistic-concurrency token carried through the hidden input on
    /// the edit form. Mismatch -> the underlying JSON handler returns
    /// 409, which we surface as a banner so the admin can refresh.
    pub expected_version: i64,
}

pub async fn library_update(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
    Form(body): Form<LibraryEditForm>,
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
    let auth = crate::auth::AuthUser(admin.claims.clone());
    let res = crate::handlers::library::update(
        State(state.clone()),
        auth,
        Path(id.clone()),
        Json(crate::handlers::library::UpdateBody {
            expected_version: body.expected_version,
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
        Ok(Json(write)) => {
            // Re-fetch live usage so the swapped-in card shows the
            // real paste count immediately - htmx replaces just this
            // slot, so anything we don't include here would read as
            // "0 uses" until the next full page reload.
            let (use_count, last_used) = sqlx::query_as::<_, (i64, Option<i64>)>(
                "SELECT COALESCE(SUM(usage_count), 0), MAX(last_used) \
                 FROM library_usage WHERE snippet_id = ?1",
            )
            .bind(&write.id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or((0, None));
            let row = LibraryRow {
                id: write.id,
                title: title.to_string(),
                body: body.body,
                tags: encode_tags_inline(&tags),
                folder_path: folder_opt,
                version: write.version,
                updated_at: write.updated_at,
                use_count,
                last_used,
            };
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (
                        header::HeaderName::from_static("hx-trigger"),
                        "libraryChanged",
                    ),
                ],
                render_library_card(&row),
            )
                .into_response()
        }
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

/// GET endpoint that returns the inline edit form for a single
/// library row. Used when the user clicks the Edit button on a card.
pub async fn library_edit_form(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Path(id): Path<String>,
) -> Response {
    match sqlx::query_as::<_, LibraryRow>(
        "SELECT id, title, body, tags, folder_path, version, updated_at \
         FROM library_snippets \
         WHERE id = ? AND is_deleted = 0",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_library_edit_form(&row),
        )
            .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<div class=\"banner error\">Snippet not found.</div>",
        )
            .into_response(),
    }
}

/// GET endpoint mirroring `library_edit_form` but returning the
/// read-only card view. Used by the Cancel button on the edit form
/// so it can swap back without losing the row.
pub async fn library_card_fragment(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Path(id): Path<String>,
) -> Response {
    match sqlx::query_as::<_, LibraryRow>(
        "SELECT id, title, body, tags, folder_path, version, updated_at \
         FROM library_snippets \
         WHERE id = ? AND is_deleted = 0",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render_library_card(&row),
        )
            .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<div class=\"banner error\">Snippet not found.</div>",
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct LibraryMoveForm {
    #[serde(default)]
    pub folder_path: String,
}

/// PUT /dashboard/library/:id/move - drag-drop endpoint. Only changes
/// folder_path, leaving title/body/tags alone. We re-use the JSON
/// update handler so version bumps + AD-version invariants stay
/// consistent, but we fetch the current row first so we can pass
/// title/body/tags through unchanged.
pub async fn library_move(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Path(id): Path<String>,
    Form(body): Form<LibraryMoveForm>,
) -> Response {
    let row = match sqlx::query_as::<_, LibraryRow>(
        "SELECT id, title, body, tags, folder_path, version, updated_at \
         FROM library_snippets WHERE id = ? AND is_deleted = 0",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let folder = body.folder_path.trim();
    let folder_opt = if folder.is_empty() {
        None
    } else {
        Some(folder.to_string())
    };
    let tags: Vec<String> = row
        .tags
        .split(',')
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
        .collect();
    let auth = crate::auth::AuthUser(admin.claims.clone());
    match crate::handlers::library::update(
        State(state.clone()),
        auth,
        Path(id),
        Json(crate::handlers::library::UpdateBody {
            expected_version: row.version,
            payload: crate::handlers::library::LibraryPayload {
                title: row.title,
                body: row.body,
                tags,
                folder_path: folder_opt,
            },
        }),
    )
    .await
    {
        // Fire libraryChanged so the sidebar's folder list reflects
        // the move (the source folder's count drops, destination
        // climbs). 204 with no body is the right shape for the
        // drag-drop response; the trigger header rides along.
        Ok(_) => (
            StatusCode::NO_CONTENT,
            [(
                header::HeaderName::from_static("hx-trigger"),
                "libraryChanged",
            )],
        )
            .into_response(),
        Err(err) => (err.status, err.message).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct FolderCreateForm {
    /// The path to create, e.g. "Replies/Billing". A path that
    /// already exists is treated as success (INSERT OR IGNORE).
    pub path: String,
}

/// Normalize a folder path: trim whitespace and outer slashes,
/// collapse internal duplicate slashes, reject empty segments and
/// control characters. Returns the normalised form on success or
/// a static error code suitable for a 400 response.
fn normalize_folder_path(input: &str) -> Result<String, &'static str> {
    let trimmed = input.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("path required");
    }
    if trimmed.contains('\n') || trimmed.contains('\r') || trimmed.contains('\t') {
        return Err("path contains invalid characters");
    }
    let mut segments: Vec<&str> = Vec::new();
    for seg in trimmed.split('/') {
        let seg = seg.trim();
        if seg.is_empty() {
            return Err("path contains an empty segment (double slash?)");
        }
        segments.push(seg);
    }
    Ok(segments.join("/"))
}

/// POST /dashboard/library/folders/create. Inserts a row into
/// library_folders for an admin-created empty folder. Lazy
/// auto-creation from snippet saves happens elsewhere; this
/// endpoint is for the "+ New folder" button. Idempotent: a
/// duplicate path becomes a no-op success.
pub async fn library_folder_create(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Form(body): Form<FolderCreateForm>,
) -> Response {
    let path = match normalize_folder_path(&body.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    // sort_order = 0 by intent: with all folders tied at 0, the
    // (sort_order, path) tiebreak in JS collapses to alphabetical,
    // so Manual mode looks identical to Alphabetical until the
    // admin actively drags something. The previous max+1 approach
    // landed new folders in creation order under Manual, which
    // reads as "Manual mode is broken" - it wasn't, just
    // unhelpful as a starting state.
    let now = Utc::now().timestamp();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO library_folders (path, sort_order, created_at) \
         VALUES (?1, 0, ?2)",
    )
    .bind(&path)
    .bind(now)
    .execute(&state.pool)
    .await;
    if let Err(e) = res {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("insert: {e}")).into_response();
    }

    // Audit. Action is per-folder; details captures the path so a
    // log scan can answer "who created Replies?"
    let actor_email = crate::audit::lookup_actor_email(&state.pool, admin.user_id()).await;
    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(admin.user_id()),
            actor_email: &actor_email,
            action: "library.folder.create",
            target_kind: Some("folder"),
            target_id: Some(&path),
            details: None,
        },
    )
    .await;

    (
        StatusCode::NO_CONTENT,
        [(
            header::HeaderName::from_static("hx-trigger"),
            "libraryChanged",
        )],
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct FolderReorderBody {
    /// Parent path for the siblings being reordered. Empty for the
    /// root level. Doesn't currently filter anything server-side -
    /// the `paths` list IS the source of truth - but logging it
    /// makes audit entries readable ("reordered the children of
    /// Replies").
    #[serde(default)]
    pub parent: String,
    /// The siblings in their new order. sort_order gets rewritten
    /// to 1..N in this exact order.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// POST /dashboard/library/folders/reorder. Rewrites sort_order
/// for the supplied list of sibling paths in left-to-right order
/// (1, 2, 3, ...). Siblings not in the list are untouched, so the
/// caller doesn't have to know about every folder in the tree -
/// just the ones being reshuffled.
///
/// Body is JSON, not form-encoded: serde_urlencoded (what axum's
/// `Form` uses) doesn't reliably deserialise repeated keys into a
/// Vec, so a `paths=A&paths=B&paths=C` form post landed with
/// `paths = []` on the server and the reorder silently no-op'd.
/// JSON sidesteps the issue.
pub async fn library_folder_reorder(
    State(state): State<AppState>,
    _admin: DashboardAdmin,
    Json(body): Json<FolderReorderBody>,
) -> Response {
    if body.paths.is_empty() {
        return (StatusCode::BAD_REQUEST, "no paths supplied").into_response();
    }
    // Cap to keep a runaway form from billing thousands of UPDATEs
    // in one POST. 500 siblings under one parent is comfortably
    // beyond any realistic library shape.
    if body.paths.len() > 500 {
        return (StatusCode::BAD_REQUEST, "too many paths in one reorder").into_response();
    }
    let mut tx = match state.pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("begin: {e}")).into_response()
        }
    };
    for (i, path) in body.paths.iter().enumerate() {
        // INSERT-OR-UPDATE pattern: a folder that exists only
        // implicitly (no library_folders row yet) gets one created
        // here at the right sort_order, instead of falling through
        // to the default 0 and being out-of-order until the next
        // explicit save.
        let order = i as i64 + 1;
        let now = Utc::now().timestamp();
        if let Err(e) = sqlx::query(
            "INSERT INTO library_folders (path, sort_order, created_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(path) DO UPDATE SET sort_order = excluded.sort_order",
        )
        .bind(path)
        .bind(order)
        .bind(now)
        .execute(&mut *tx)
        .await
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("update: {e}")).into_response();
        }
    }
    if let Err(e) = tx.commit().await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("commit: {e}")).into_response();
    }

    // Intentionally not recorded in the audit log: folder reorder is
    // a pure UX-state change with no destructive effect, and the
    // serialised order list is noise that would crowd out the
    // mutations operators actually want to see.

    (
        StatusCode::NO_CONTENT,
        [(
            header::HeaderName::from_static("hx-trigger"),
            "libraryChanged",
        )],
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct FolderMoveForm {
    /// The folder being moved, e.g. "Replies/Billing".
    pub old: String,
    /// Destination path. Either a top-level name ("Outreach") or a
    /// new parent slash-path ("Sales/Outreach"). Empty string means
    /// "move to root" (un-nest).
    pub new: String,
}

/// POST /dashboard/library/folders/move - nest/unnest/rename a
/// whole folder by rewriting `folder_path` for every snippet whose
/// path equals `old` or starts with `old/`. One audit row per
/// move (not per affected snippet) so the log stays readable for
/// big mass-rename operations.
pub async fn library_folder_move(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    Form(body): Form<FolderMoveForm>,
) -> Response {
    let old = body.old.trim().trim_matches('/');
    let new = body.new.trim().trim_matches('/');

    // Reject empty source - "drop nothing" isn't a real op and we
    // don't want a typo turning into "rename root to <something>".
    if old.is_empty() {
        return (StatusCode::BAD_REQUEST, "old path required").into_response();
    }
    // Sanitise the destination. Empty is fine (means root); otherwise
    // reject paths with double-slash or non-printable junk.
    if !new.is_empty() && (new.contains("//") || new.contains('\n') || new.contains('\r')) {
        return (StatusCode::BAD_REQUEST, "invalid new path").into_response();
    }
    // Detect the "rename a folder into itself or its descendant"
    // case. Without this, dragging Billing onto Billing/Refunds
    // would generate Billing/Refunds/Billing/Refunds/... in a loop
    // that the LIKE prefix UPDATE would then mangle.
    if !new.is_empty() && (new == old || new.starts_with(&format!("{old}/"))) {
        return (
            StatusCode::BAD_REQUEST,
            "can't move a folder into itself or one of its descendants",
        )
            .into_response();
    }

    // Compute every snippet that this rename touches in one read,
    // then UPDATE one by one so each row gets a unique version
    // bump in the global library-version stream. Doing the bumps
    // off a pre-fetched MAX(version) avoids re-scanning the table
    // N times.
    let prefix = format!("{old}/");
    let like_pattern = format!("{prefix}%");
    let mut tx = match state.pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("begin: {e}")).into_response();
        }
    };
    let affected: Vec<(String, String)> = match sqlx::query_as(
        "SELECT id, folder_path FROM library_snippets \
         WHERE is_deleted = 0 \
           AND (folder_path = ?1 OR folder_path LIKE ?2) \
         ORDER BY id",
    )
    .bind(old)
    .bind(&like_pattern)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("select: {e}")).into_response();
        }
    };
    if affected.is_empty() {
        // Empty source folder = noop success. Lets the JS treat it
        // as "we moved nothing" rather than 404.
        return StatusCode::NO_CONTENT.into_response();
    }
    let base_version: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM library_snippets")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let now = Utc::now().timestamp();

    for (i, (id, current)) in affected.iter().enumerate() {
        let new_path = if current == old {
            // The folder itself: empty `new` means "move to root"
            // (no folder); anything else becomes the literal new
            // path.
            new.to_string()
        } else {
            // Descendant: peel off the old prefix, glue on the new
            // one. When new is empty (un-nest to root), drop the
            // prefix and keep the suffix as the new top-level path.
            let suffix = &current[prefix.len()..];
            if new.is_empty() {
                suffix.to_string()
            } else {
                format!("{new}/{suffix}")
            }
        };
        let folder_value: Option<&str> = if new_path.is_empty() {
            None
        } else {
            Some(new_path.as_str())
        };
        let v = base_version + 1 + i as i64;
        if let Err(e) = sqlx::query(
            "UPDATE library_snippets \
             SET folder_path = ?1, version = ?2, updated_at = ?3 \
             WHERE id = ?4",
        )
        .bind(folder_value)
        .bind(v)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("update: {e}")).into_response();
        }
    }

    // Keep library_folders in sync with the rename. Two updates:
    //   1. Exact-match: the source folder itself becomes the new
    //      target (delete row if un-nesting to root and the new
    //      name is empty - but normalize_folder_path would have
    //      rejected that case, so we just rename).
    //   2. Descendants: prefix-replace via SUBSTR. SQLite has no
    //      REPLACE_PREFIX, so build the new path inline with
    //      string concatenation.
    // We don't bump sort_order here; the relative order within
    // the new parent stays whatever it was. ON CONFLICT DO NOTHING
    // guards against "rename Replies to Outreach when Outreach
    // already exists" - the snippets land under the existing
    // Outreach row and the source row is dropped.
    if new.is_empty() {
        // Un-nest to root: descendants of `old` keep their suffix
        // as their new top-level path. The folder rows for the
        // old paths get rewritten to just the suffix.
        let _ = sqlx::query(
            "UPDATE OR REPLACE library_folders \
             SET path = SUBSTR(path, ?1 + 1) \
             WHERE path = ?2 OR path LIKE ?3",
        )
        .bind(prefix.len() as i64)
        .bind(old)
        .bind(&like_pattern)
        .execute(&mut *tx)
        .await;
        // The exact-match row (path = old) is special-cased: its
        // suffix would be empty (un-nesting "Foo" to root), which
        // would orphan the row to an empty path. Delete it instead.
        let _ = sqlx::query("DELETE FROM library_folders WHERE path = ?1")
            .bind(old)
            .execute(&mut *tx)
            .await;
        // Mirror the rewrite into the audit log so historical
        // "created folder Foo" rows link to wherever Foo lives
        // now instead of a stale path that no longer exists.
        // Descendants follow the same SUBSTR rule the folder rows
        // used above.
        let _ = sqlx::query(
            "UPDATE audit_log \
             SET target_id = SUBSTR(target_id, ?1 + 1) \
             WHERE target_kind = 'folder' AND target_id LIKE ?2",
        )
        .bind(prefix.len() as i64)
        .bind(&like_pattern)
        .execute(&mut *tx)
        .await;
        // Exact-match audit rows: the folder itself just became
        // root - its snippets are no longer in any folder, so
        // there's nothing to link to. Empty target_id collapses
        // to a dash in humanize_audit_target.
        let _ = sqlx::query(
            "UPDATE audit_log SET target_id = '' \
             WHERE target_kind = 'folder' AND target_id = ?1",
        )
        .bind(old)
        .execute(&mut *tx)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE OR REPLACE library_folders \
             SET path = ?1 || SUBSTR(path, ?2 + 1) \
             WHERE path LIKE ?3",
        )
        .bind(format!("{new}/"))
        .bind(prefix.len() as i64)
        .bind(&like_pattern)
        .execute(&mut *tx)
        .await;
        let _ = sqlx::query("UPDATE OR REPLACE library_folders SET path = ?1 WHERE path = ?2")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await;
        // Mirror the rewrite into the audit log: any historical
        // row whose target was `old` or one of its descendants
        // gets re-pointed at the new path so the link in the
        // Target column lands on the current location.
        let _ = sqlx::query(
            "UPDATE audit_log \
             SET target_id = ?1 || SUBSTR(target_id, ?2 + 1) \
             WHERE target_kind = 'folder' AND target_id LIKE ?3",
        )
        .bind(format!("{new}/"))
        .bind(prefix.len() as i64)
        .bind(&like_pattern)
        .execute(&mut *tx)
        .await;
        let _ = sqlx::query(
            "UPDATE audit_log SET target_id = ?1 \
             WHERE target_kind = 'folder' AND target_id = ?2",
        )
        .bind(new)
        .bind(old)
        .execute(&mut *tx)
        .await;
    }

    if let Err(e) = tx.commit().await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("commit: {e}")).into_response();
    }

    // One audit row per folder move (not per snippet), with the
    // count so an admin can sanity-check what was touched. The
    // target_id reflects the post-move location so the link
    // navigates to where the folder ended up; root moves carry an
    // empty target_id which collapses to a dash in the table.
    let actor_email = crate::audit::lookup_actor_email(&state.pool, admin.user_id()).await;
    crate::audit::record(
        &state.pool,
        crate::audit::AuditEvent {
            actor_id: Some(admin.user_id()),
            actor_email: &actor_email,
            action: "library.folder.move",
            target_kind: Some("folder"),
            target_id: Some(new),
            details: Some(serde_json::json!({
                "from": old,
                "to": new,
                "snippets_moved": affected.len(),
            })),
        },
    )
    .await;

    (
        StatusCode::NO_CONTENT,
        [(
            header::HeaderName::from_static("hx-trigger"),
            "libraryChanged",
        )],
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
        // HX-Trigger fires a custom event on document.body once the
        // delete settles. The library sidebar's hx-trigger listens
        // for it ("libraryChanged from:body"), so the folder list
        // refreshes immediately instead of waiting for the 10s
        // poll tick.
        Ok(_) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (
                    header::HeaderName::from_static("hx-trigger"),
                    "libraryChanged",
                ),
            ],
            "",
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

// ---- /dashboard/audit (admin activity log) ----

#[derive(Debug, Deserialize)]
pub struct AuditPageQuery {
    /// Zero-based page index. Each page shows `per_page` entries in
    /// reverse-chronological order. Capped to AUDIT_MAX_PAGES so a
    /// runaway URL doesn't waste a SELECT against the index.
    #[serde(default)]
    pub page: Option<i64>,
    /// Per-page size. Allowlisted to AUDIT_PAGE_CHOICES so the URL
    /// parameter can't ask for a 10000-row dump; anything outside
    /// the list falls back to AUDIT_DEFAULT_PAGE_SIZE.
    #[serde(default)]
    pub per_page: Option<i64>,
}

/// Choices we let the admin pick from in the dropdown. Defaulting
/// to 25 keeps the page short on first load; an operator combing
/// recent activity can bump to 100 in one click.
const AUDIT_PAGE_CHOICES: &[i64] = &[25, 50, 100, 200];
const AUDIT_DEFAULT_PAGE_SIZE: i64 = 25;
/// Hard cap so a runaway `?page=99999` doesn't blow time on a
/// pointless OFFSET. The dashboard never advertises a page above
/// (total / per_page); this just shields the SELECT.
const AUDIT_MAX_PAGES: i64 = 200;

/// Clamp a user-supplied per_page to the allowlist. Anything not
/// on the list (or absent) collapses to the default. Centralised
/// so the validator can't disagree with the dropdown.
fn audit_page_size(req: Option<i64>) -> i64 {
    match req {
        Some(n) if AUDIT_PAGE_CHOICES.contains(&n) => n,
        _ => AUDIT_DEFAULT_PAGE_SIZE,
    }
}

pub async fn audit_page(
    State(state): State<AppState>,
    admin: DashboardAdmin,
    axum::extract::Query(q): axum::extract::Query<AuditPageQuery>,
) -> Response {
    let session = DashboardSession {
        claims: admin.claims.clone(),
    };
    let per_page = audit_page_size(q.per_page);
    let page = q.page.unwrap_or(0).clamp(0, AUDIT_MAX_PAGES);
    let offset = page * per_page;
    // Total + page count drive both the "page N of M" display and
    // the next-link condition. The old `rows.len() == PAGE_SIZE`
    // heuristic only worked once the first page was completely
    // full, which made the pager look broken on small instances.
    // COUNT must apply the same hidden-action filter as list_recent
    // or the pager's "page N of M" claim disagrees with what
    // actually appears in the table.
    let count_sql = format!(
        "SELECT COUNT(*) FROM audit_log{}",
        crate::audit::hidden_actions_filter_sql("WHERE"),
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let total_pages = if total == 0 {
        1
    } else {
        ((total + per_page - 1) / per_page).min(AUDIT_MAX_PAGES + 1)
    };
    let rows = crate::audit::list_recent(&state.pool, per_page, offset).await;

    // Pre-fetch display_name + email for every user target on
    // this page so the target column renders as "Name <email>"
    // instead of opaque uuids. One IN-list SELECT per page is
    // cheap; the alternative (per-row lookup in humanize) would
    // be O(N) round-trips.
    let mut user_target_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        if r.target_kind.as_deref() == Some("user") {
            if let Some(uid) = r.target_id.as_deref() {
                if !uid.is_empty() {
                    user_target_ids.insert(uid.to_string());
                }
            }
        }
    }
    let users_by_id: std::collections::HashMap<String, (String, String)> =
        if user_target_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            // Manual placeholder list because sqlx::query_as doesn't
            // expand a Vec into ? placeholders for SQLite. Each id
            // is a uuid string, safe to inline after escape_html-
            // style sanitisation - but a placeholder loop is cleaner.
            let placeholders: Vec<&str> = user_target_ids.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, display_name, email FROM users WHERE id IN ({})",
                placeholders.join(",")
            );
            let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
            for uid in &user_target_ids {
                q = q.bind(uid);
            }
            q.fetch_all(&state.pool)
                .await
                .map(|rows| {
                    rows.into_iter()
                        .map(|(id, name, email)| (id, (name, email)))
                        .collect()
                })
                .unwrap_or_default()
        };

    // Same pre-fetch pattern for library snippet targets: pull the
    // CURRENT folder_path and is_deleted state in one IN query so
    // the target column can link straight to the snippet's card,
    // not the library home page. A snippet's folder may have
    // changed since the audit row was written; the live row wins.
    let mut snippet_target_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for r in &rows {
        if r.target_kind.as_deref() == Some("library") {
            if let Some(sid) = r.target_id.as_deref() {
                if !sid.is_empty() {
                    snippet_target_ids.insert(sid.to_string());
                }
            }
        }
    }
    let snippets_by_id: std::collections::HashMap<String, (Option<String>, bool)> =
        if snippet_target_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let placeholders: Vec<&str> = snippet_target_ids.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, folder_path, is_deleted FROM library_snippets WHERE id IN ({})",
                placeholders.join(",")
            );
            let mut q = sqlx::query_as::<_, (String, Option<String>, i64)>(&sql);
            for sid in &snippet_target_ids {
                q = q.bind(sid);
            }
            q.fetch_all(&state.pool)
                .await
                .map(|rows| {
                    rows.into_iter()
                        .map(|(id, fp, d)| (id, (fp, d != 0)))
                        .collect()
                })
                .unwrap_or_default()
        };

    let mut body = String::new();
    body.push_str("<h1>Audit log</h1>");
    body.push_str(
        "<p class=\"muted\">Every admin mutation (user + library writes) is recorded here. \
         Append-only; entries don't expire. Sorted newest first.</p>",
    );

    if rows.is_empty() {
        body.push_str("<p class=\"muted\">No audit entries yet.</p>");
    } else {
        body.push_str("<table class=\"audit-table\"><thead><tr>");
        body.push_str("<th>When</th><th>Actor</th><th>Action</th><th>Target</th><th>Details</th>");
        body.push_str("</tr></thead><tbody>");
        for r in &rows {
            let details_html = humanize_audit_details(&r.action, r.details.as_deref());
            let target_html = humanize_audit_target(
                r.target_kind.as_deref(),
                r.target_id.as_deref(),
                &users_by_id,
                &snippets_by_id,
            );
            // Parse the details JSON once per row so we can decide
            // whether to surface the diff toggle without re-parsing
            // inside humanize_audit_details.
            let parsed: Option<serde_json::Value> = r
                .details
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let diff_block = if r.action == "library.update" {
                parsed.as_ref().and_then(render_library_update_diff)
            } else {
                None
            };
            let toggle_btn = if diff_block.is_some() {
                format!(
                    "<button type=\"button\" class=\"audit-diff-toggle\" \
                             aria-controls=\"audit-diff-{id}\" \
                             aria-expanded=\"false\" \
                             onclick=\"toggleAuditDiff({id})\">+ diff</button> ",
                    id = r.id,
                )
            } else {
                String::new()
            };
            body.push_str(&format!(
                "<tr>\
                   <td class=\"audit-when\">{when}</td>\
                   <td>{actor}</td>\
                   <td>{action}</td>\
                   <td>{target}</td>\
                   <td class=\"audit-details\">{toggle}{details}</td>\
                 </tr>",
                when = format_relative(r.at),
                actor = escape_html(&r.actor_email),
                action = humanize_audit_action(&r.action),
                target = target_html,
                toggle = toggle_btn,
                details = details_html,
            ));
            if let Some(diff_html) = diff_block {
                body.push_str(&format!(
                    "<tr class=\"audit-diff-row\" id=\"audit-diff-{id}\" hidden>\
                       <td colspan=\"5\" class=\"audit-diff-cell\">{diff}</td>\
                     </tr>",
                    id = r.id,
                    diff = diff_html,
                ));
            }
        }
        body.push_str("</tbody></table>");
        // Inline toggle so the diff row reveals when the user clicks
        // the "+ diff" button. Kept inline (vs. a static .js) because
        // it's the only JS on the page and saves one round-trip.
        body.push_str(
            "<script>\
               function toggleAuditDiff(id) {\
                 var row = document.getElementById('audit-diff-' + id);\
                 if (!row) return;\
                 var btn = document.querySelector('button[aria-controls=\"audit-diff-' + id + '\"]');\
                 var open = row.hidden;\
                 row.hidden = !open;\
                 if (btn) {\
                   btn.setAttribute('aria-expanded', open ? 'true' : 'false');\
                   btn.textContent = open ? '- diff' : '+ diff';\
                 }\
               }\
             </script>",
        );
    }

    body.push_str("<div class=\"audit-pager\">");
    if page > 0 {
        body.push_str(&format!(
            "<a href=\"/dashboard/audit?page={}&per_page={}\">&larr; Newer</a>",
            page - 1,
            per_page,
        ));
    } else {
        body.push_str("<span class=\"muted\">&larr; Newer</span>");
    }
    body.push_str(&format!(
        " <span class=\"muted small\">page {} of {} ({} {})</span> ",
        page + 1,
        total_pages,
        total,
        if total == 1 { "entry" } else { "entries" },
    ));
    let has_next = (page + 1) < total_pages && page < AUDIT_MAX_PAGES;
    if has_next {
        body.push_str(&format!(
            "<a href=\"/dashboard/audit?page={}&per_page={}\">Older &rarr;</a>",
            page + 1,
            per_page,
        ));
    } else {
        body.push_str("<span class=\"muted\">Older &rarr;</span>");
    }
    // Per-page picker. Inline form so changing the value reloads
    // the page at p=0 with the new size (jumping back to the
    // newest entries because the old offset wouldn't make sense
    // at the new page size). No JS required - the browser's
    // native form submit handles it.
    body.push_str(
        "<form method=\"get\" action=\"/dashboard/audit\" class=\"audit-perpage\">\
           <label for=\"audit-perpage-select\">Per page:</label>\
           <select id=\"audit-perpage-select\" name=\"per_page\" \
                   onchange=\"this.form.submit()\">",
    );
    for choice in AUDIT_PAGE_CHOICES {
        let selected = if *choice == per_page { " selected" } else { "" };
        body.push_str(&format!(
            "<option value=\"{choice}\"{selected}>{choice}</option>"
        ));
    }
    body.push_str(
        "</select>\
           <input type=\"hidden\" name=\"page\" value=\"0\" />\
           <noscript><button type=\"submit\" class=\"btn\">Apply</button></noscript>\
         </form>",
    );
    body.push_str("</div>");

    render_page(&state, &session, "Audit", NavTab::Audit, &body)
        .await
        .into_response()
}

/// Pretty-print the dotted action code: "user.update" → "User update".
/// Keeps the raw form available in the table title attribute so an
/// operator wanting to grep logs can still find the exact code.
fn humanize_audit_action(action: &str) -> String {
    let (kind, verb) = action.split_once('.').unwrap_or((action, ""));
    let kind_title = capitalize(kind);
    let verb_title = if verb.is_empty() {
        String::new()
    } else {
        format!(" {verb}")
    };
    format!(
        "<span title=\"{raw}\">{kind}{verb}</span>",
        raw = escape_html(action),
        kind = escape_html(&kind_title),
        verb = escape_html(&verb_title),
    )
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Render the target cell as a clickable link to the relevant
/// dashboard page when we know how to navigate to one. Users get
/// links to their detail page; library snippets link to the
/// library list (no per-snippet detail page exists). Anything
/// else falls back to the raw "kind:id" string.
/// Display label for a single audit-row user target: prefer
/// "Name <email>" pulled from the users table, fall back to the
/// uuid for accounts the SELECT didn't see (e.g. deleted users
/// the cascade hasn't NULL'd yet, or pre-fetch failures).
fn user_target_label(
    users: &std::collections::HashMap<String, (String, String)>,
    uid: &str,
) -> String {
    match users.get(uid) {
        Some((name, email)) => format!("{} &lt;{}&gt;", escape_html(name), escape_html(email)),
        None => format!("user {}", escape_html(uid)),
    }
}

fn humanize_audit_target(
    kind: Option<&str>,
    id: Option<&str>,
    users: &std::collections::HashMap<String, (String, String)>,
    snippets: &std::collections::HashMap<String, (Option<String>, bool)>,
) -> String {
    match (kind, id) {
        // User target gets the user's display_name + email when
        // available so an operator scanning the log doesn't have
        // to cross-reference UUIDs to people. Falls back to the
        // raw uuid only when the lookup misses.
        (Some("user"), Some(uid)) if !uid.is_empty() => format!(
            "<a href=\"/dashboard/users/{uid_attr}\">{label}</a>",
            uid_attr = escape_html(uid),
            label = user_target_label(users, uid),
        ),
        // Library snippet: link to the specific card by anchoring
        // on its DOM id, and pre-filter the library to whichever
        // folder the snippet currently lives in so the card is
        // actually on the page when the anchor jump lands. Lookup
        // misses (deleted or never-fetched) fall back to a muted
        // "(deleted)" label rather than a broken link.
        (Some("library"), Some(lid)) if !lid.is_empty() => match snippets.get(lid) {
            Some((_, true)) => format!(
                "<span class=\"muted\" title=\"deleted snippet {lid}\">library snippet (deleted)</span>",
                lid = escape_html(lid),
            ),
            Some((folder, false)) => {
                let folder_param = folder
                    .as_deref()
                    .filter(|f| !f.is_empty())
                    .map(|f| format!("?folder={}", urlencoding::encode(f)))
                    .unwrap_or_default();
                format!(
                    "<a href=\"/dashboard/library{folder}#lib-{id_attr}\" \
                        title=\"library snippet {lid}\">library snippet</a>",
                    folder = folder_param,
                    id_attr = escape_html(lid),
                    lid = escape_html(lid),
                )
            }
            // Pre-fetch missed (race with delete, or id not in the
            // snippets table at all). Fall back to the library home
            // so the link still goes somewhere useful.
            None => format!(
                "<a href=\"/dashboard/library\" title=\"library snippet {lid}\">library snippet</a>",
                lid = escape_html(lid),
            ),
        },
        // Folder targets navigate straight to the folder's filtered
        // library view via ?folder= - useful for folder.move and
        // similar actions where the admin wants to see the result.
        // The query value is URL-encoded so slashes in nested
        // paths ride through cleanly across browsers.
        (Some("folder"), Some(fp)) if !fp.is_empty() => format!(
            "<a href=\"/dashboard/library?folder={fp_attr}\">folder <em>{fp_display}</em></a>",
            fp_attr = urlencoding::encode(fp),
            fp_display = escape_html(fp),
        ),
        // Empty id (e.g. library.folder.move into root fires with
        // target_id="") shouldn't render as "folder:" or "user:"
        // with nothing trailing - those read as broken UI.
        // Collapse to a dash like the no-target case.
        (Some(_), Some("")) => "<span class=\"muted\">-</span>".to_string(),
        (Some(k), Some(i)) => format!("{}:{}", escape_html(k), escape_html(i)),
        _ => "<span class=\"muted\">-</span>".to_string(),
    }
}

/// Translate the JSON `details` blob into a human-readable sentence
/// per action. Falls back to the raw JSON (still HTML-escaped, in
/// <code>) for any action we don't have a formatter for - so an
/// operator never loses information, just gets nicer copy for the
/// common cases.
fn humanize_audit_details(action: &str, details: Option<&str>) -> String {
    let Some(s) = details.filter(|s| !s.is_empty()) else {
        return "<span class=\"muted\">-</span>".to_string();
    };
    let parsed: Option<serde_json::Value> = serde_json::from_str(s).ok();
    let Some(json) = parsed else {
        return format!("<code>{}</code>", escape_html(s));
    };
    let get_str = |key: &str| -> Option<String> {
        json.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let get_from_to = |key: &str| -> Option<(String, String)> {
        let obj = json.get(key)?;
        let from = obj.get("from")?;
        let to = obj.get("to")?;
        Some((value_to_string(from), value_to_string(to)))
    };
    match action {
        "user.create" => {
            let email = get_str("email").unwrap_or_default();
            let role = get_str("role").unwrap_or_default();
            format!(
                "Created <strong>{}</strong> as {}",
                escape_html(&email),
                escape_html(&role)
            )
        }
        "user.update" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some((from, to)) = get_from_to("role") {
                parts.push(format!(
                    "role {} &rarr; <strong>{}</strong>",
                    escape_html(&from),
                    escape_html(&to)
                ));
            }
            if let Some((from, to)) = get_from_to("is_disabled") {
                // booleans round-trip through value_to_string as
                // "true"/"false"; translate to the language an
                // admin actually thinks in.
                let pretty = |b: String| -> String {
                    match b.as_str() {
                        "true" => "disabled".to_string(),
                        "false" => "active".to_string(),
                        _ => b,
                    }
                };
                parts.push(format!(
                    "status {} &rarr; <strong>{}</strong>",
                    escape_html(&pretty(from)),
                    escape_html(&pretty(to))
                ));
            }
            if parts.is_empty() {
                "<span class=\"muted\">no fields changed</span>".to_string()
            } else {
                parts.join(", ")
            }
        }
        "user.delete" => {
            let email = get_str("email").unwrap_or_default();
            if email.is_empty() {
                "Deleted account".to_string()
            } else {
                format!("Deleted <strong>{}</strong>", escape_html(&email))
            }
        }
        "library.create" => {
            let title = get_str("title").unwrap_or_default();
            let folder = get_str("folder_path").unwrap_or_default();
            if folder.is_empty() {
                format!("Created \"<strong>{}</strong>\"", escape_html(&title))
            } else {
                format!(
                    "Created \"<strong>{}</strong>\" in <em>{}</em>",
                    escape_html(&title),
                    escape_html(&folder)
                )
            }
        }
        "library.update" => {
            // Dropped the trailing "(vN)" - version is an internal
            // wire-protocol counter and isn't user-meaningful.
            let title = get_str("title").unwrap_or_default();
            let folder = get_str("folder_path").unwrap_or_default();
            if folder.is_empty() {
                format!("Updated \"<strong>{}</strong>\"", escape_html(&title))
            } else {
                format!(
                    "Updated \"<strong>{}</strong>\" in <em>{}</em>",
                    escape_html(&title),
                    escape_html(&folder)
                )
            }
        }
        "library.delete" => "Deleted library snippet".to_string(),
        "library.folder.move" => {
            let from = get_str("from").unwrap_or_default();
            let to = get_str("to").unwrap_or_default();
            let count = json
                .get("snippets_moved")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let dest = if to.is_empty() {
                "<em>(root)</em>".to_string()
            } else {
                format!("<em>{}</em>", escape_html(&to))
            };
            let suffix = if count > 0 {
                format!(" ({count} snippet{})", if count == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            format!(
                "Moved folder <strong>{}</strong> to {}{}",
                escape_html(&from),
                dest,
                suffix,
            )
        }
        "library.folder.create" => {
            // No details on this action today; the path is in
            // target_id (which the target column already shows).
            "Created folder".to_string()
        }
        "library.folder.reorder" => {
            let parent = get_str("parent").unwrap_or_default();
            let parent_display = if parent.is_empty() {
                "root level".to_string()
            } else {
                format!("<em>{}</em>", escape_html(&parent))
            };
            // Show the new order as leaf names so the line reads
            // naturally instead of as a slash-path soup.
            let names: Vec<String> = json
                .get("order")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
                        .collect()
                })
                .unwrap_or_default();
            if names.is_empty() {
                format!("Reordered children of {parent_display}")
            } else {
                let list = escape_html(&names.join(", "));
                format!("Reordered {parent_display}: {list}")
            }
        }
        // Unknown action - keep the raw JSON, but escaped + in a
        // <code> block so the table column doesn't break HTML.
        _ => format!("<code>{}</code>", escape_html(s)),
    }
}

/// JSON value -> short display string. Bools render as
/// "true"/"false"; strings unwrap; numbers display as-is; nulls
/// and objects fall back to compact JSON.
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

#[derive(Copy, Clone)]
enum DiffKind {
    Same,
    Add,
    Del,
}

/// Line-level LCS diff. O(n*m) DP is fine: audit details cap each
/// field at 16 KB so n*m tops out around 200*200 lines worst case.
/// Returns lines in source order with their kind.
fn line_diff(a: &str, b: &str) -> Vec<(DiffKind, String)> {
    let a_lines: Vec<&str> = a.split('\n').collect();
    let b_lines: Vec<&str> = b.split('\n').collect();
    let n = a_lines.len();
    let m = b_lines.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    // enumerate() on both axes so clippy doesn't flag the
    // index-into-Vec pattern; we still need i + 1 / j + 1 to
    // step the DP table.
    for (i, a_line) in a_lines.iter().enumerate() {
        for (j, b_line) in b_lines.iter().enumerate() {
            dp[i + 1][j + 1] = if a_line == b_line {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut i = n;
    let mut j = m;
    let mut out: Vec<(DiffKind, String)> = Vec::new();
    while i > 0 && j > 0 {
        if a_lines[i - 1] == b_lines[j - 1] {
            out.push((DiffKind::Same, a_lines[i - 1].to_string()));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            out.push((DiffKind::Del, a_lines[i - 1].to_string()));
            i -= 1;
        } else {
            out.push((DiffKind::Add, b_lines[j - 1].to_string()));
            j -= 1;
        }
    }
    while i > 0 {
        out.push((DiffKind::Del, a_lines[i - 1].to_string()));
        i -= 1;
    }
    while j > 0 {
        out.push((DiffKind::Add, b_lines[j - 1].to_string()));
        j -= 1;
    }
    out.reverse();
    out
}

/// Render one field's before/after as a small diff block. Returns
/// None when the two sides are byte-equal so the caller can skip
/// emitting a section for fields that didn't change.
fn render_diff_field(label: &str, before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    let mut out = String::new();
    out.push_str(&format!(
        "<div class=\"diff-field\">\
           <div class=\"diff-field-label\">{}</div>\
           <pre class=\"diff-block\">",
        escape_html(label)
    ));
    for (kind, text) in line_diff(before, after) {
        let (cls, marker) = match kind {
            DiffKind::Same => ("diff-line-same", " "),
            DiffKind::Add => ("diff-line-add", "+"),
            DiffKind::Del => ("diff-line-del", "-"),
        };
        out.push_str(&format!(
            "<span class=\"diff-line {cls}\">\
               <span class=\"diff-marker\">{marker}</span>{text}</span>\n",
            cls = cls,
            marker = marker,
            text = escape_html(&text),
        ));
    }
    out.push_str("</pre></div>");
    Some(out)
}

/// Build the expandable diff block for a library.update audit row.
/// Returns None for rows recorded before the before/after capture
/// landed (older entries have no `before` key) or for an edit that
/// changed nothing on any of the diffable fields.
fn render_library_update_diff(details: &serde_json::Value) -> Option<String> {
    let before = details.get("before")?;
    let after = details.get("after")?;
    let get_s = |obj: &serde_json::Value, key: &str| -> String {
        obj.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let tags_str = |obj: &serde_json::Value| -> String {
        obj.get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let mut sections = String::new();
    if let Some(s) = render_diff_field("Title", &get_s(before, "title"), &get_s(after, "title")) {
        sections.push_str(&s);
    }
    if let Some(s) = render_diff_field(
        "Folder",
        &get_s(before, "folder_path"),
        &get_s(after, "folder_path"),
    ) {
        sections.push_str(&s);
    }
    if let Some(s) = render_diff_field("Tags", &tags_str(before), &tags_str(after)) {
        sections.push_str(&s);
    }
    if let Some(s) = render_diff_field("Body", &get_s(before, "body"), &get_s(after, "body")) {
        sections.push_str(&s);
    }
    if sections.is_empty() {
        return None;
    }
    Some(sections)
}
