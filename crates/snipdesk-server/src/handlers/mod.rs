//! HTTP handler modules. Each module owns one logical concern; the
//! router in `http.rs` stitches them together.

pub mod admin;
pub mod auth;
pub mod library;
pub mod oidc;
pub mod snippets;
