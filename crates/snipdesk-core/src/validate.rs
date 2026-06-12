//! Size and character limits for snippet data, enforced on every
//! write path. Mirrored in crates/snipdesk-server/src/validate.rs;
//! keep the constants in lockstep so a snippet one side accepts
//! can't 400 on the other.

pub const TITLE_MAX_CHARS: usize = 300;
pub const BODY_MAX_CHARS: usize = 100_000;
pub const TAG_MAX_CHARS: usize = 60;
pub const MAX_TAGS: usize = 50;
/// Full path, separators included.
pub const FOLDER_MAX_CHARS: usize = 300;

// Bodies allow \n \r \t; one-line fields reject all control chars.
fn forbidden_in_body(c: char) -> bool {
    c.is_control() && !matches!(c, '\n' | '\r' | '\t')
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_snippets() {
        assert!(validate_snippet(
            "Greeting",
            "Hello!\r\n\tHow can I help?",
            &["billing".to_string()],
            Some("Replies/English"),
        )
        .is_ok());
    }

    #[test]
    fn accepts_multibyte_text_up_to_the_char_limit() {
        let title = "\u{305b}".repeat(TITLE_MAX_CHARS);
        assert!(validate_title(&title).is_ok());
        let over = "\u{305b}".repeat(TITLE_MAX_CHARS + 1);
        assert!(validate_title(&over).is_err());
    }

    #[test]
    fn rejects_oversized_body() {
        let body = "a".repeat(BODY_MAX_CHARS + 1);
        assert!(validate_body(&body).is_err());
        let ok = "a".repeat(BODY_MAX_CHARS);
        assert!(validate_body(&ok).is_ok());
    }

    #[test]
    fn rejects_control_characters() {
        assert!(validate_title("null\u{0}byte").is_err());
        assert!(validate_title("two\nlines").is_err());
        assert!(validate_body("escape\u{1b}[31m sequence").is_err());
        assert!(validate_body("but newlines\nand tabs\tare fine").is_ok());
        assert!(validate_tags(&["bad\ttag".to_string()]).is_err());
        assert!(validate_folder(Some("a\u{0}b")).is_err());
    }

    #[test]
    fn rejects_empty_and_whitespace_titles() {
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
    }

    #[test]
    fn rejects_tag_overflow() {
        let many: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect();
        assert!(validate_tags(&many).is_err());
        let long = "x".repeat(TAG_MAX_CHARS + 1);
        assert!(validate_tags(&[long]).is_err());
    }
}
