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
    Html(render(
        LAYOUT,
        &[
            ("TITLE", title),
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
            ("NAV_USER", &escape_html(&display)),
            ("NAV_ROLE", &escape_html(&role)),
            ("CONTENT", content),
        ],
    ))
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
    body.push_str("<div class=\"library-layout\">");
    body.push_str(
        "<aside class=\"library-sidebar\" id=\"library-sidebar\" \
        hx-get=\"/dashboard/library/folders\" \
        hx-trigger=\"every 10s [document.querySelector('.lib-edit-form') === null], libraryChanged from:body\" \
        hx-swap=\"innerHTML\" hx-include=\"#library-folder-input\">",
    );
    body.push_str(&render_library_folder_tree(&rows, &selected));
    body.push_str("</aside>");
    body.push_str("<div class=\"library-main\">");
    // Hidden input mirrors the current folder so polling sweeps the
    // right view. htmx's hx-include picks it up and appends ?folder=.
    body.push_str(&format!(
        "<input type=\"hidden\" id=\"library-folder-input\" name=\"folder\" value=\"{}\" />",
        escape_html(&selected),
    ));
    body.push_str(&library_create_form(&selected));
    // Polls every 5s so another admin's adds / edits / deletes surface
    // without a manual refresh. The folder filter rides along via
    // hx-include on the hidden input above. The JS-expression gate on
    // the trigger skips the poll when an inline edit form is open,
    // otherwise the next tick would wipe whatever the admin was
    // typing - the poll swaps the whole tbody's innerHTML, including
    // their half-finished edit.
    body.push_str(
        "<div class=\"library-list\" id=\"library-list\" \
              hx-get=\"/dashboard/library/cards\" \
              hx-trigger=\"every 5s [document.querySelector('.lib-edit-form') === null], libraryChanged from:body\" \
              hx-include=\"#library-folder-input\" \
              hx-swap=\"innerHTML\">",
    );
    let filtered = filter_library_rows(&rows, &selected);
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

/// Build the sidebar folder list. Top three pseudo-nodes are All,
/// Unfiled, then the actual folders sorted alphabetically. Each node
/// carries `data-folder-path` so the drag-and-drop JS can wire up
/// drop targets. Counts shown inline are direct (not recursive) so
/// "Billing" shows snippets right at /Billing not under
/// /Billing/Refunds - matches the desktop client's behaviour.
fn render_library_folder_tree(rows: &[LibraryRow], selected: &str) -> String {
    // Direct counts per folder, plus the running total.
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut unfiled = 0i64;
    let mut all = 0i64;
    for r in rows {
        all += 1;
        match r.folder_path.as_deref() {
            None | Some("") => unfiled += 1,
            Some(fp) => *counts.entry(fp.to_string()).or_insert(0) += 1,
        }
    }
    let mut out = String::new();
    out.push_str("<div class=\"lib-folder-header\">Folders</div>");
    out.push_str(&render_lib_folder_node(
        FOLDER_ALL,
        "All snippets",
        all,
        selected == FOLDER_ALL,
        false,
    ));
    out.push_str(&render_lib_folder_node(
        FOLDER_UNFILED,
        "Unfiled",
        unfiled,
        selected == FOLDER_UNFILED,
        false,
    ));
    for (path, count) in counts {
        out.push_str(&render_lib_folder_node(
            &path,
            &path,
            count,
            selected == path,
            true,
        ));
    }
    out
}

fn render_lib_folder_node(
    path: &str,
    label: &str,
    count: i64,
    active: bool,
    droppable: bool,
) -> String {
    let active_class = if active { " active" } else { "" };
    let drop_attrs = if droppable {
        "data-droppable=\"1\""
    } else if path == FOLDER_UNFILED {
        // Unfiled is also a valid drop target: dragging a card here
        // means "clear this snippet's folder_path".
        "data-droppable=\"1\" data-unfiled=\"1\""
    } else {
        ""
    };
    format!(
        "<a class=\"lib-folder-node{active_class}\" \
            href=\"/dashboard/library?folder={href}\" \
            data-folder-path=\"{path_attr}\" {drop_attrs}>\
           <span class=\"label\">{label_safe}</span>\
           <span class=\"count\">{count}</span>\
         </a>",
        href = escape_html(path),
        path_attr = escape_html(path),
        label_safe = escape_html(label),
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
    let filtered = filter_library_rows(&rows, &selected);
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
    let selected = library_selected_folder(&q.folder);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        render_library_folder_tree(&rows, &selected),
    )
        .into_response()
}

/// Shared body of the cards container; same output whether we're
/// rendering the initial page or a polling refresh.
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
    format!(
        "<div class=\"library-card\" id=\"lib-{id_attr}\" \
             draggable=\"true\" data-snippet-id=\"{id_attr}\">\
           <div class=\"card-head\">\
             <span class=\"drag-handle\" title=\"Drag to move\">::</span>\
             <span class=\"title\">{title}</span>{folder}{tags}{usage_pill}\
             <span class=\"meta\">v{ver} | updated {when}</span>\
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
        ver = r.version,
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
}

/// Inline drag-drop + formatting-toolbar wiring for the library page.
/// Scoped via `data-*` attributes on the library DOM so a stray
/// global keypress can't trigger formatting on an unrelated input.
const LIBRARY_PAGE_JS: &str = r#"<script>
(function () {
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

  // ---- Drag-drop: move snippet into a folder ----
  var dragging = null;
  document.body.addEventListener("dragstart", function (e) {
    var card = e.target.closest && e.target.closest(".library-card[data-snippet-id]");
    if (!card) return;
    dragging = card.getAttribute("data-snippet-id");
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", dragging);
    card.classList.add("dragging");
  });
  document.body.addEventListener("dragend", function (e) {
    var card = e.target.closest && e.target.closest(".library-card");
    if (card) card.classList.remove("dragging");
    dragging = null;
    document.querySelectorAll(".lib-folder-node.drop-target").forEach(function (n) {
      n.classList.remove("drop-target");
    });
  });
  document.body.addEventListener("dragover", function (e) {
    var node = e.target.closest && e.target.closest(".lib-folder-node[data-droppable]");
    if (!node) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    node.classList.add("drop-target");
  });
  document.body.addEventListener("dragleave", function (e) {
    var node = e.target.closest && e.target.closest(".lib-folder-node");
    if (node) node.classList.remove("drop-target");
  });
  document.body.addEventListener("drop", function (e) {
    var node = e.target.closest && e.target.closest(".lib-folder-node[data-droppable]");
    if (!node || !dragging) return;
    e.preventDefault();
    node.classList.remove("drop-target");
    var target = node.getAttribute("data-folder-path") || "";
    // "Unfiled" maps to empty folder_path on the server side.
    var folder = node.hasAttribute("data-unfiled") ? "" : target;
    var fd = new FormData();
    fd.append("folder_path", folder);
    fetch("/dashboard/library/" + encodeURIComponent(dragging) + "/move", {
      method: "PUT",
      body: fd,
    }).then(function (r) {
      if (!r.ok) { console.warn("move failed", r.status); return; }
      // Re-fetch the cards + sidebar so the moved snippet relocates
      // visually and the counts redraw.
      if (window.htmx) {
        var list = document.getElementById("library-list");
        var sidebar = document.getElementById("library-sidebar");
        if (list) window.htmx.trigger(list, "refresh-now");
        if (sidebar) window.htmx.trigger(sidebar, "refresh-now");
      }
    });
  });

  // htmx custom trigger so the JS above can ask the polling
  // endpoints to fire on demand without waiting for the 5s tick.
  if (window.htmx) {
    document.body.addEventListener("htmx:configRequest", function (e) {
      // no-op hook for future request annotation
    });
    document.querySelectorAll("[hx-trigger]").forEach(function (el) {
      var t = el.getAttribute("hx-trigger");
      if (t && t.indexOf("refresh-now") === -1) {
        el.setAttribute("hx-trigger", t + ", refresh-now");
      }
    });
  }
})();
</script>"#;

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
