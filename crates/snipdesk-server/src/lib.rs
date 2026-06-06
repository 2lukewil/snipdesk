//! Library crate re-export. Lets integration tests in `tests/` reach the
//! handler modules without going through `main.rs`. The binary entrypoint
//! (`src/main.rs`) still drives the production build via `clap`.

pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod handlers;
pub mod http;
