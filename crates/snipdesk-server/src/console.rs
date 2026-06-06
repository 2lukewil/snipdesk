//! Interactive console for the running server.
//!
//! Spawned by `main.rs::run` when stdin is a TTY. Reads commands a
//! line at a time and dispatches them against the same SQLite pool
//! the HTTP layer is using. Modelled on Minecraft's server console -
//! you type into the terminal that started the server; log output
//! interleaves with your typing (that's the price of not pulling in a
//! line-editor crate like rustyline). For complex sessions, the
//! standalone `snipdesk-server users <cmd>` subcommand still works
//! from a separate shell.
//!
//! Why not headless mode: when stdin is NOT a TTY (running under
//! systemd, Docker without `-it`, a CI runner, etc.), reading stdin
//! would either block forever or eat the first byte of whatever else
//! the process group writes there. The TTY check at the call site
//! gates this whole module out in those environments.
//!
//! What the console can do:
//!   - `help` - list commands
//!   - `users list / promote / demote / disable / enable / delete`
//!   - `stop` / `quit` / `exit` - graceful shutdown
//!
//! What the console deliberately can't do:
//!   - `users reset-password` - its prompt reads from stdin, which
//!     would race with the console's own stdin reader. The standalone
//!     subcommand handles it cleanly; the console refuses with a
//!     pointer.
//!   - `users delete <email>` without `--yes` - same reason: the
//!     interactive confirmation needs stdin. Force the `--yes`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use sqlx::SqlitePool;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use crate::cli::{self, UsersCmd};

/// Top-level shape parsed from one console line. clap's derive does
/// all the heavy lifting - quoted args, --flag handling, structured
/// errors. We prepend a dummy "console" as argv[0] before calling
/// `try_parse_from` so clap's binary-name expectation is satisfied.
#[derive(Parser, Debug)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    // We define our own `help` subcommand (prints a one-screen cheat
    // sheet, not clap's verbose default). Suppress clap's auto-
    // generated one to avoid the "command `help` is duplicated"
    // panic at startup.
    disable_help_subcommand = true
)]
struct ConsoleLine {
    #[command(subcommand)]
    cmd: ConsoleCmd,
}

#[derive(Subcommand, Debug)]
enum ConsoleCmd {
    /// Show the list of available commands.
    Help,
    /// User management - same subcommand tree as `snipdesk-server
    /// users` from the shell, minus reset-password (stdin clash).
    Users {
        #[command(subcommand)]
        cmd: UsersCmd,
    },
    /// Gracefully stop the server. Drains in-flight HTTP requests
    /// before exiting.
    Stop,
    /// Alias for `stop`.
    Quit,
    /// Alias for `stop`.
    Exit,
}

/// Drive the console loop until the user types `stop` or stdin closes
/// (Ctrl+D on Unix, Ctrl+Z+Enter on Windows). Owns the shutdown
/// `oneshot::Sender`; once fired, the server's `with_graceful_shutdown`
/// future resolves and `axum::serve` returns.
pub async fn run(pool: SqlitePool, shutdown_tx: oneshot::Sender<()>) {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    // Use eprintln! for console UI text - keeps it on stderr, distinct
    // from any stdout-only operators might want to capture (none today,
    // but future `--json` modes won't fight the banner).
    eprintln!("Console ready. Type `help` for commands; `stop` to shut down.");

    let mut tx = Some(shutdown_tx);
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Err(e) = handle_line(&pool, line, &mut tx).await {
                    eprintln!("error: {e}");
                }
                // `tx` is taken once `stop` fires; from that point the
                // console exits its loop and lets the runtime drain.
                if tx.is_none() {
                    break;
                }
            }
            Ok(None) => {
                // EOF on stdin - treat as a soft shutdown signal so a
                // detached terminal doesn't pin the server alive.
                eprintln!("stdin closed; shutting down.");
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
                break;
            }
            Err(e) => {
                eprintln!("console stdin read error: {e}");
                break;
            }
        }
    }
}

async fn handle_line(
    pool: &SqlitePool,
    line: &str,
    shutdown: &mut Option<oneshot::Sender<()>>,
) -> Result<()> {
    let argv: Vec<&str> = line.split_whitespace().collect();
    let parsed = match ConsoleLine::try_parse_from(&argv) {
        Ok(p) => p,
        Err(e) => {
            // clap renders its own friendly diagnostics - let it print
            // straight to the terminal instead of wrapping in our own
            // error formatting.
            let _ = e.print();
            return Ok(());
        }
    };

    match parsed.cmd {
        ConsoleCmd::Help => {
            print_help();
            Ok(())
        }
        ConsoleCmd::Users { cmd } => dispatch_users(pool, cmd).await,
        ConsoleCmd::Stop | ConsoleCmd::Quit | ConsoleCmd::Exit => {
            if let Some(tx) = shutdown.take() {
                eprintln!("Stopping...");
                // send() can only fail if the receiver dropped first,
                // which would be a programmer bug; ignore.
                let _ = tx.send(());
            }
            Ok(())
        }
    }
}

/// Filter out the user commands whose stdin needs clash with the
/// console reader, then delegate to the shared dispatcher.
async fn dispatch_users(pool: &SqlitePool, cmd: UsersCmd) -> Result<()> {
    match &cmd {
        UsersCmd::ResetPassword { .. } => {
            eprintln!("`users reset-password` is not available in the console - its");
            eprintln!("interactive password prompt would race with the console's stdin reader.");
            eprintln!(
                "Run from a separate shell, e.g.:\n  \
                 snipdesk-server -c snipdesk-server.toml users reset-password <email>"
            );
            Ok(())
        }
        UsersCmd::Delete { yes: false, .. } => {
            eprintln!("In the console, `users delete` needs `--yes` (no interactive confirm).");
            eprintln!("Re-run as: users delete <email> --yes");
            Ok(())
        }
        _ => cli::run_with_pool(pool, cmd).await,
    }
}

fn print_help() {
    eprintln!("Commands:");
    eprintln!("  users list");
    eprintln!("  users promote <email>");
    eprintln!("  users demote <email>");
    eprintln!("  users disable <email>");
    eprintln!("  users enable <email>");
    eprintln!("  users delete <email> --yes");
    eprintln!("  stop                       (aliases: quit, exit)");
    eprintln!("  help");
    eprintln!();
    eprintln!(
        "Not available in console (stdin conflict - use a separate shell):\n  \
         users reset-password <email>"
    );
}
