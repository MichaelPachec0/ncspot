//! Secrets removed from debug log lines: bearer tokens, token fields and authorization values. `--log-secrets`
//! turns this off.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    // Each field name may be followed by a quote, and each quote may be backslash-escaped
    // (JSON logged inside a string). An authorization value keeps its scheme word.
    Regex::new(concat!(
        r#"(?i)("#,
        r#"bearer\s+"#,
        r#"|(?:access_token|refresh_token)\\?"?\s*[:=]\s*\\?"?"#,
        r#"|authorization\\?"?\s*[:=]\s*\\?"?(?:[A-Za-z]+\s+)?"#,
        r#")[A-Za-z0-9._~+/=-]+"#,
    ))
    .expect("secret pattern")
});

/// `line` with every bearer token, token field value and authorization value replaced by
/// `[redacted]`.
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
    fn masks_backslash_escaped_json_token_fields() {
        let line = r#"body: "{\"access_token\":\"abc.DEF-1\",\"refresh_token\": \"r_2\"}""#;
        let out = redact_secrets(line);
        assert!(!out.contains("abc.DEF-1"), "{out}");
        assert!(!out.contains("r_2"), "{out}");
        assert!(out.contains(r#"\"access_token\":\"[redacted]\""#), "{out}");
    }

    #[test]
    fn masks_lower_case_bearer() {
        let out = redact_secrets("header bearer BQAW7Upl-Z9");
        assert!(!out.contains("BQAW7Upl"));
        assert!(out.contains("bearer [redacted]"), "{out}");
    }

    #[test]
    fn masks_authorization_values_in_any_scheme() {
        let out = redact_secrets("authorization: Basic dXNlcjpwYXNz");
        assert!(!out.contains("dXNlcjpwYXNz"));
        assert!(out.contains("authorization: Basic [redacted]"), "{out}");

        let out = redact_secrets(r#"{"authorization":"tok-123","x":1}"#);
        assert!(!out.contains("tok-123"));
        assert!(
            out.contains(r#""authorization":"[redacted]","x":1"#),
            "{out}"
        );

        let out = redact_secrets("Authorization: Bearer BQAW7Upl");
        assert!(!out.contains("BQAW7Upl"));
        assert!(out.contains("Bearer [redacted]"), "{out}");
    }

    #[test]
    fn leaves_clean_lines_borrowed() {
        assert!(matches!(
            redact_secrets("fetching user playlists, offset: 50"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
