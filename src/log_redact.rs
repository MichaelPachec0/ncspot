//! Secrets removed from debug log lines: bearer tokens and token fields. `--log-secrets`
//! turns this off.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(bearer\s+|(?:access_token|refresh_token)"?\s*[:=]\s*"?)[A-Za-z0-9._~+/=-]+"#)
        .expect("secret pattern")
});

/// `line` with every bearer token and token field value replaced by `[redacted]`.
pub fn redact_secrets(line: &str) -> Cow<'_, str> {
    SECRET.replace_all(line, "${1}[redacted]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_bearer_tokens_and_token_fields() {
        let line = r#"GET /v1/me [authorization: Bearer BQAW7Upl-Z9_z.4] {"access_token":"abc.DEF-1","refresh_token": "r_2", "expires_in": 3600} access_token=xyz"#;
        let out = redact_secrets(line);
        assert!(!out.contains("BQAW7Upl"));
        assert!(!out.contains("abc.DEF-1"));
        assert!(!out.contains("r_2"));
        assert!(!out.contains("xyz"));
        assert!(out.contains("Bearer [redacted]"));
        assert!(
            out.contains("\"expires_in\": 3600"),
            "other fields untouched: {out}"
        );
    }

    #[test]
    fn leaves_clean_lines_borrowed() {
        assert!(matches!(
            redact_secrets("fetching user playlists, offset: 50"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
