//! HTTP handler modules. Each module owns one logical concern; the
//! router in `http.rs` stitches them together.

pub mod auth;
pub mod snippets;
