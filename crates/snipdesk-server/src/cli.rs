//! CLI user-management commands.
//!
//! These let an operator promote/demote/disable/delete users and reset
//! passwords directly from the server host, without going through the
//! dashboard. Useful for:
//!   - Recovery: an admin locked themselves out (e.g. forgot password,
//!     dashboard misconfigured, OIDC misroute). Shell access on the
//!     server box is the disaster-recovery channel.
//!   - Ops automation: scripted user provisioning during onboarding
//!     without driving the dashboard.
//!
//! The commands open the same SQLite database the running server uses.
//! SQLite's WAL mode handles a concurrent reader+writer fine, so it's
//! safe to run these while the server is up. We deliberately don't
//! bother with a lockfile or "stop the server first" - that would just
//! introduce friction with no real benefit at single-file-DB scale.
//!
//! Last-admin protection (can't demote / disable / delete the only
//! remaining admin) is enforced here too. The CLI has no "user" doing
//! actions, so self-protection rules from the JSON layer don't apply;
//! only last-admin matters.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use chrono::{TimeZone, Utc};
use clap::Subcommand;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::ConnectOptions;
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum UsersCmd {
    /// List every account with role, status, snippet count, and last
    /// seen.
    List,
    /// Promote a user to admin. No-op if they're already admin.
    Promote {
        /// Email of the target user (case-insensitive).
        email: String,
    },
    /// Demote an admin to member. Refuses if it would leave zero
    /// admins remaining.
    Demote { email: String },
    /// Disable an account. Disabled users can still authenticate
    /// against the DB (so the JWT they hold remains technically valid
    /// until expiry), but the API + dashboard reject their requests.
    Disable { email: String },
    /// Re-enable a previously disabled account.
    Enable { email: String },
    /// Permanently delete a user. Cascades to their personal snippets
    /// (FK ON DELETE CASCADE). Refuses to delete the last admin. Asks
    /// for `yes` confirmation unless `--yes` is passed.
    Delete {
        email: String,
        /// Skip the interactive confirmation prompt - for scripted
        /// runs where the operator is already sure.
        #[arg(long)]
        yes: bool,
    },
    /// Set a new password for a user. Reads the password from stdin so
    /// it doesn't land in shell history. Useful when the user forgot
    /// their password or the recovery path otherwise fell over.
    ResetPassword {
        email: String,
        /// Skip the "type new password twice" prompt; read a single
        /// line from stdin. For scripted use; do NOT pipe a literal
        /// from a shell command - that puts the plaintext in history.
        #[arg(long)]
        from_stdin: bool,
    },
    /// Diagnostic dump for one user: id, role, raw snippet count (live
    /// + tombstoned), and the first few snippet IDs. Used to confirm
    ///   rows landed in the right `owner_id` when the dashboard count
    ///   disagrees with what a client said it uploaded.
    Info { email: String },
}

/// Entry point dispatched by main.rs's `Cmd::Users { cmd }` arm - opens
/// its own SQLite pool against `data_dir/snipdesk.db`.
pub async fn run(data_dir: &Path, cmd: UsersCmd) -> Result<()> {
    let pool = open_pool(data_dir).await?;
    run_with_pool(&pool, cmd).await
}

/// Same as `run` but takes an already-open pool. The interactive
/// console (see `console.rs`) calls this so it can hit the same DB
/// the running server is already using, without spinning up a second
/// connection pool just for command dispatch.
pub async fn run_with_pool(pool: &SqlitePool, cmd: UsersCmd) -> Result<()> {
    match cmd {
        UsersCmd::List => list(pool).await,
        UsersCmd::Promote { email } => set_role(pool, &email, "admin").await,
        UsersCmd::Demote { email } => set_role(pool, &email, "member").await,
        UsersCmd::Disable { email } => set_disabled(pool, &email, true).await,
        UsersCmd::Enable { email } => set_disabled(pool, &email, false).await,
        UsersCmd::Delete { email, yes } => delete(pool, &email, yes).await,
        UsersCmd::ResetPassword { email, from_stdin } => {
            reset_password(pool, &email, from_stdin).await
        }
        UsersCmd::Info { email } => info(pool, &email).await,
    }
}

/// Open the same SQLite DB the server uses. We don't go through
/// `db::open` because that runs migrations - fine to run again
/// (migrations are idempotent) but slower and noisier than we need for
/// a one-shot CLI command. The CLI assumes the schema is already up.
async fn open_pool(data_dir: &Path) -> Result<SqlitePool> {
    let db_path = data_dir.join("snipdesk.db");
    if !db_path.exists() {
        bail!(
            "no database found at {}. Start the server once first to \
             initialise the schema, then re-run this command.",
            db_path.display()
        );
    }
    let url = format!("sqlite://{}", db_path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .log_statements(tracing::log::LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("connect to {url}"))?;
    Ok(pool)
}

// ---- helpers ----

#[derive(sqlx::FromRow)]
struct UserRow {
    id: String,
    email: String,
    display_name: String,
    role: String,
    is_disabled: i64,
    last_seen_at: Option<i64>,
    snippet_count: i64,
}

async fn find_by_email(pool: &SqlitePool, email: &str) -> Result<UserRow> {
    let email = email.trim().to_lowercase();
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         WHERE u.email = ? \
         GROUP BY u.id",
    )
    .bind(&email)
    .fetch_optional(pool)
    .await?;
    row.ok_or_else(|| anyhow!("no user with email '{email}'"))
}

async fn admin_count(pool: &SqlitePool) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_disabled = 0")
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// Bail if removing this user from the admin pool (by demote, disable,
/// or delete) would leave the org with zero functioning admins.
/// "Functioning" excludes disabled accounts - a disabled admin can't
/// actually administrate, so we shouldn't count them.
async fn guard_last_admin(pool: &SqlitePool, target: &UserRow, action: &str) -> Result<()> {
    if target.role != "admin" {
        return Ok(());
    }
    // If the target is already disabled and the caller is *deleting*
    // or *demoting*, the active-admin count didn't include them
    // anyway. But for clarity we still surface the check uniformly.
    let active_admins = admin_count(pool).await?;
    let counts_as_active = target.is_disabled == 0;
    if counts_as_active && active_admins <= 1 {
        bail!(
            "refusing to {action} {email} - they're the only active admin. \
             Promote another user first.",
            email = target.email
        );
    }
    Ok(())
}

// ---- commands ----

// Clippy flags the literal header values as "could be inlined into the
// format string." Keeping them as positional args is what makes the
// header line up with the data row that follows - both use the same
// width-padded format string. Inlining would force the header into a
// separate hand-padded string and create two sources of truth for
// column widths.
#[allow(clippy::print_literal)]
async fn list(pool: &SqlitePool) -> Result<()> {
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, u.role, u.is_disabled, \
                u.last_seen_at, \
                COALESCE(SUM(CASE WHEN s.is_deleted = 0 THEN 1 ELSE 0 END), 0) AS snippet_count \
         FROM users u \
         LEFT JOIN personal_snippets s ON s.owner_id = u.id \
         GROUP BY u.id \
         ORDER BY u.role DESC, u.email ASC",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        println!("(no users yet - the first signup auto-promotes to admin)");
        return Ok(());
    }

    // Compute column widths so the table doesn't shimmy as content
    // shifts between rows. tabular text beats CSV for human reading.
    let email_w = rows.iter().map(|r| r.email.len()).max().unwrap_or(5).max(5);
    let name_w = rows
        .iter()
        .map(|r| r.display_name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<email_w$}  {:<name_w$}  {:<6}  {:<8}  {:>8}  {}",
        "EMAIL",
        "NAME",
        "ROLE",
        "STATUS",
        "SNIPPETS",
        "LAST SEEN",
        email_w = email_w,
        name_w = name_w,
    );
    for r in &rows {
        let status = if r.is_disabled != 0 {
            "disabled"
        } else {
            "active"
        };
        let last_seen = match r.last_seen_at {
            None => "never".to_string(),
            Some(ts) => Utc
                .timestamp_opt(ts, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "?".to_string()),
        };
        println!(
            "{:<email_w$}  {:<name_w$}  {:<6}  {:<8}  {:>8}  {}",
            r.email,
            r.display_name,
            r.role,
            status,
            r.snippet_count,
            last_seen,
            email_w = email_w,
            name_w = name_w,
        );
    }
    Ok(())
}

async fn set_role(pool: &SqlitePool, email: &str, role: &str) -> Result<()> {
    let user = find_by_email(pool, email).await?;
    if user.role == role {
        println!("{email} is already {role}; no change.");
        return Ok(());
    }
    if role == "member" {
        guard_last_admin(pool, &user, "demote").await?;
    }
    sqlx::query("UPDATE users SET role = ? WHERE id = ?")
        .bind(role)
        .bind(&user.id)
        .execute(pool)
        .await?;
    let action = if role == "admin" {
        "promoted"
    } else {
        "demoted"
    };
    println!("{} {email} → {role}.", capitalize(action));
    Ok(())
}

async fn set_disabled(pool: &SqlitePool, email: &str, disabled: bool) -> Result<()> {
    let user = find_by_email(pool, email).await?;
    if (user.is_disabled != 0) == disabled {
        let state = if disabled { "disabled" } else { "active" };
        println!("{email} is already {state}; no change.");
        return Ok(());
    }
    if disabled {
        guard_last_admin(pool, &user, "disable").await?;
    }
    sqlx::query("UPDATE users SET is_disabled = ? WHERE id = ?")
        .bind(if disabled { 1 } else { 0 })
        .bind(&user.id)
        .execute(pool)
        .await?;
    let verb = if disabled { "disabled" } else { "enabled" };
    println!("{} {email}.", capitalize(verb));
    Ok(())
}

async fn delete(pool: &SqlitePool, email: &str, skip_confirm: bool) -> Result<()> {
    let user = find_by_email(pool, email).await?;
    guard_last_admin(pool, &user, "delete").await?;

    if !skip_confirm {
        print!(
            "Delete {email} permanently? This will remove {n} snippet(s). \
             Type 'yes' to confirm: ",
            n = user.snippet_count
        );
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let res = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&user.id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        bail!("delete affected 0 rows - did someone else delete them first?");
    }
    println!("Deleted {email}.");
    Ok(())
}

async fn reset_password(pool: &SqlitePool, email: &str, from_stdin: bool) -> Result<()> {
    let user = find_by_email(pool, email).await?;

    // Always prefer rpassword-style no-echo input via `rpassword` if
    // it's available - for v1 we read from stdin plaintext. The
    // `--from-stdin` flag reads a single line silently (operator
    // controls echo via their tty); without it we prompt twice with a
    // confirmation check, with plaintext echo (visible) so the user
    // can catch typos. Tradeoff: visible echo isn't ideal but adding
    // a `rpassword`-style dep just for this one CLI command is over-
    // engineering for v1. Disposable shell sessions are fine.
    let new_pw = if from_stdin {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else {
        print!("New password (min 10 chars): ");
        io::stdout().flush().ok();
        let mut first = String::new();
        io::stdin().read_line(&mut first)?;
        let first = first.trim_end_matches(['\r', '\n']).to_string();

        print!("Repeat: ");
        io::stdout().flush().ok();
        let mut second = String::new();
        io::stdin().read_line(&mut second)?;
        let second = second.trim_end_matches(['\r', '\n']).to_string();
        if first != second {
            bail!("passwords don't match - aborting.");
        }
        first
    };

    if new_pw.len() < 10 {
        bail!("password must be at least 10 characters.");
    }

    // Inline the hash here rather than calling crate::auth::hash_password
    // - that returns ApiError which is shaped for HTTP responses; cleaner
    // boundary if we don't drag that type into the CLI module.
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(new_pw.as_bytes(), &salt)
        .map_err(|e| anyhow!("argon2 hash failed: {e}"))?
        .to_string();

    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hash)
        .bind(&user.id)
        .execute(pool)
        .await?;
    println!(
        "Reset password for {email}. Their existing JWT(s) remain valid \
         until expiry (24h) - rotate the server's jwt_secret if you need \
         immediate invalidation."
    );
    Ok(())
}

/// Dump everything we know about one user, from the rawest possible
/// angle. The dashboard / `users list` both compute counts via a
/// LEFT JOIN + SUM, which is *almost* always right but can be confusing
/// if a row's `owner_id` somehow doesn't match what you expect. This
/// command runs three independent counts (live, tombstoned, total) and
/// shows the first ten snippet IDs so you can cross-check by hand.
async fn info(pool: &SqlitePool, email: &str) -> Result<()> {
    let user = find_by_email(pool, email).await?;
    println!("user");
    println!("  id:           {}", user.id);
    println!("  email:        {}", user.email);
    println!("  display_name: {}", user.display_name);
    println!("  role:         {}", user.role);
    println!(
        "  is_disabled:  {}",
        if user.is_disabled != 0 { "yes" } else { "no" }
    );
    println!(
        "  last_seen_at: {}",
        match user.last_seen_at {
            None => "never".to_string(),
            Some(ts) => Utc
                .timestamp_opt(ts, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "?".to_string()),
        }
    );

    // Three independent counts, no JOIN: read straight off
    // personal_snippets to rule out a JOIN-pred bug or a stale GROUP BY.
    let (live,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM personal_snippets WHERE owner_id = ? AND is_deleted = 0",
    )
    .bind(&user.id)
    .fetch_one(pool)
    .await?;
    let (tombstoned,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM personal_snippets WHERE owner_id = ? AND is_deleted = 1",
    )
    .bind(&user.id)
    .fetch_one(pool)
    .await?;
    let (total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM personal_snippets WHERE owner_id = ?")
            .bind(&user.id)
            .fetch_one(pool)
            .await?;

    println!();
    println!("personal_snippets (raw)");
    println!("  live:       {live}");
    println!("  tombstoned: {tombstoned}");
    println!("  total:      {total}");

    // First ten ids, oldest first. Useful for spot-checking that the
    // ids match what the desktop client thinks it pushed.
    let ids: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT id, version, is_deleted FROM personal_snippets \
         WHERE owner_id = ? ORDER BY created_at ASC LIMIT 10",
    )
    .bind(&user.id)
    .fetch_all(pool)
    .await?;
    if !ids.is_empty() {
        println!("  first ten ids (oldest first):");
        for (id, version, is_deleted) in ids {
            let tomb = if is_deleted != 0 { " [tombstone]" } else { "" };
            println!("    v{version:3}  {id}{tomb}");
        }
    }

    // Also check: are there any snippets where owner_id LOOKS LIKE
    // this user's id but isn't an exact match? A whitespace bug or a
    // case mismatch in UUIDs would show up here.
    let (fuzzy,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM personal_snippets \
         WHERE owner_id LIKE ? AND owner_id <> ?",
    )
    .bind(format!("%{}%", user.id))
    .bind(&user.id)
    .fetch_one(pool)
    .await?;
    if fuzzy > 0 {
        println!();
        println!(
            "WARNING: {fuzzy} snippet(s) have an owner_id that *contains* but doesn't \
             *equal* this user's id. JOIN-mismatch bug; investigate."
        );
    }

    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
