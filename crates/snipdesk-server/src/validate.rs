//! Size and character limits for user-supplied content.
//!
//! Mirror of `crates/snipdesk-core/src/validate.rs` - the server
//! crate deliberately doesn't depend on snipdesk-core (it would drag
//! rusqlite and the desktop paste stack into the Docker image), so
//! the rules live twice. Keep the constants and semantics in
//! lockstep: a snippet the client accepts must sync without a 400,
//! and vice versa.

pub const TITLE_MAX_CHARS: usize = 300;
pub const BODY_MAX_CHARS: usize = 100_000;
pub const TAG_MAX_CHARS: usize = 60;
pub const MAX_TAGS: usize = 50;
pub const FOLDER_MAX_CHARS: usize = 300;
pub const DISPLAY_NAME_MAX_CHARS: usize = 100;

/// Control characters with no business in stored text. Newlines,
/// carriage returns, and tabs are legitimate in multi-line bodies;
/// everything else control-class is rejected everywhere.
fn forbidden_in_body(c: char) -> bool {
    c.is_control() && !matches!(c, '\n' | '\r' | '\t')
}

/// One-line fields (titles, tags, folder paths, names) additionally
/// reject newlines and tabs.
fn forbidden_in_line(c: char) -> bool {
    c.is_control()
}

pub fn validate_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("title is required".to_string());
    }
    let n = title.chars().count();
    if n > TITLE_MAX_CHARS {
        return Err(format!(
            "title is too long ({n} characters; max {TITLE_MAX_CHARS})"
        ));
    }
    if title.chars().any(forbidden_in_line) {
        return Err("title contains control characters".to_string());
    }
    Ok(())
}

pub fn validate_body(body: &str) -> Result<(), String> {
    let n = body.chars().count();
    if n > BODY_MAX_CHARS {
        return Err(format!(
            "snippet body is too long ({n} characters; max {BODY_MAX_CHARS})"
        ));
    }
    if body.chars().any(forbidden_in_body) {
        return Err("snippet body contains control characters".to_string());
    }
    Ok(())
}

pub fn validate_tags(tags: &[String]) -> Result<(), String> {
    if tags.len() > MAX_TAGS {
        return Err(format!("too many tags ({}; max {MAX_TAGS})", tags.len()));
    }
    for t in tags {
        let n = t.chars().count();
        if n > TAG_MAX_CHARS {
            return Err(format!(
                "tag is too long ({n} characters; max {TAG_MAX_CHARS})"
            ));
        }
        if t.chars().any(forbidden_in_line) {
            return Err("tag contains control characters".to_string());
        }
    }
    Ok(())
}

pub fn validate_folder(folder_path: Option<&str>) -> Result<(), String> {
    let Some(p) = folder_path else { return Ok(()) };
    let n = p.chars().count();
    if n > FOLDER_MAX_CHARS {
        return Err(format!(
            "folder path is too long ({n} characters; max {FOLDER_MAX_CHARS})"
        ));
    }
    if p.chars().any(forbidden_in_line) {
        return Err("folder path contains control characters".to_string());
    }
    Ok(())
}

/// The whole-snippet check used by every payload-accepting endpoint
/// (personal-snippet sync, library API, dashboard forms and import).
pub fn validate_snippet(
    title: &str,
    body: &str,
    tags: &[String],
    folder_path: Option<&str>,
) -> Result<(), String> {
    validate_title(title)?;
    validate_body(body)?;
    validate_tags(tags)?;
    validate_folder(folder_path)?;
    Ok(())
}

/// Signup path: reject rather than mangle - the user typed the name
/// and can fix it.
pub fn validate_display_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("display_name is required".to_string());
    }
    let n = name.chars().count();
    if n > DISPLAY_NAME_MAX_CHARS {
        return Err(format!(
            "display name is too long ({n} characters; max {DISPLAY_NAME_MAX_CHARS})"
        ));
    }
    if name.chars().any(forbidden_in_line) {
        return Err("display name contains control characters".to_string());
    }
    Ok(())
}

/// OIDC path: the name comes from the identity provider, not the
/// user's keyboard, so failing the whole sign-in over a weird claim
/// would be hostile. Strip control characters and clamp instead.
pub fn sanitize_display_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !forbidden_in_line(*c))
        .take(DISPLAY_NAME_MAX_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "user".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_limits_match_the_client() {
        assert!(validate_snippet("Greeting", "Hello!\nThanks", &[], Some("Replies")).is_ok());
        assert!(validate_title(&"x".repeat(TITLE_MAX_CHARS + 1)).is_err());
        assert!(validate_body(&"x".repeat(BODY_MAX_CHARS + 1)).is_err());
        assert!(validate_title("null\u{0}byte").is_err());
        assert!(validate_body("escape\u{1b}seq").is_err());
        assert!(validate_body("tabs\tand\r\nnewlines ok").is_ok());
        assert!(validate_folder(Some("a\nb")).is_err());
    }

    #[test]
    fn display_name_rules() {
        assert!(validate_display_name("Lucas Wilson").is_ok());
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name(&"x".repeat(DISPLAY_NAME_MAX_CHARS + 1)).is_err());
        assert!(validate_display_name("two\nlines").is_err());
    }

    #[test]
    fn sanitize_clamps_instead_of_rejecting() {
        assert_eq!(sanitize_display_name("Normal Name"), "Normal Name");
        assert_eq!(sanitize_display_name("ctrl\u{0}chars\u{1b}"), "ctrlchars");
        assert_eq!(sanitize_display_name("  \u{0} "), "user");
        assert_eq!(
            sanitize_display_name(&"y".repeat(500)).chars().count(),
            DISPLAY_NAME_MAX_CHARS
        );
    }
}
