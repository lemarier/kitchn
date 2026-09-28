//! Redaction for text that Orca returns and Kitchen may surface in errors.
//!
//! Orca messages can echo command arguments, including dispatch capabilities
//! and forge tokens. Every Orca-provided string that reaches an error goes
//! through [`redact`] first.

/// Longest redacted message kept, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;

const REDACTED: &str = "[redacted]";

/// Token prefixes that always mark a credential.
const SECRET_PREFIXES: [&str; 13] = [
    "dcap_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "sk-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    // A JSON Web Token's base64 header, `{"`.
    "eyJ",
];

/// Minimum length of an unrecognized token that is treated as a secret.
const OPAQUE_SECRET_BYTES: usize = 32;

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'/' | b'=' | b'.')
}

fn is_uuid(token: &str) -> bool {
    let groups: Vec<&str> = token.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_secret(token: &str) -> bool {
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix))
    {
        return true;
    }
    // Long opaque values mixing letters and digits look like keys, including
    // base64 with `/`. Absolute paths, file names, and UUIDs are kept so
    // errors stay diagnosable.
    token.len() >= OPAQUE_SECRET_BYTES
        && !token.starts_with('/')
        && !token.contains('.')
        && !is_uuid(token)
        && token.bytes().any(|byte| byte.is_ascii_digit())
        && token.bytes().any(|byte| byte.is_ascii_alphabetic())
}

/// Replace credential-like tokens and truncate to [`MAX_MESSAGE_BYTES`].
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_MESSAGE_BYTES));
    let mut rest = text;
    while !rest.is_empty() && out.len() < MAX_MESSAGE_BYTES {
        let token_len = rest.bytes().take_while(|byte| is_token_byte(*byte)).count();
        if token_len > 0 {
            let (token, tail) = rest.split_at(token_len);
            out.push_str(if is_secret(token) { REDACTED } else { token });
            rest = tail;
        } else {
            let mut chars = rest.chars();
            if let Some(ch) = chars.next() {
                out.push(if ch.is_control() { ' ' } else { ch });
            }
            rest = chars.as_str();
        }
    }
    if out.len() > MAX_MESSAGE_BYTES {
        let mut end = MAX_MESSAGE_BYTES;
        while !out.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        out.truncate(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_credential_prefixes_are_removed() {
        let text = "failed --dispatch-capability dcap_abc123 with gho_Tok3n and sk-live";
        assert_eq!(
            redact(text),
            "failed --dispatch-capability [redacted] with [redacted] and [redacted]"
        );
    }

    #[test]
    fn identifiers_and_paths_survive() {
        let text = "Worker Dispatch ctx_22f22ac0905e in /Users/me/work-tree 3b198225-d6b1-4602-be8b-5d72a08c8c61";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn long_opaque_tokens_are_removed() {
        let text = "token a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8 end";
        assert_eq!(redact(text), "token [redacted] end");
        let base64 = "key QWxhZGRpbjpvcGVuIHNlc2FtZQ/9x+Zk3Ltq8Rr2Wm== end";
        assert_eq!(redact(base64), "key [redacted] end");
        let jwt = "bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOjF9.c2ln end";
        assert_eq!(redact(jwt), "bearer [redacted] end");
    }

    #[test]
    fn output_is_bounded_and_control_characters_are_flattened() {
        let long = "x ".repeat(MAX_MESSAGE_BYTES);
        assert!(redact(&long).len() <= MAX_MESSAGE_BYTES);
        assert_eq!(redact("a\nb\u{1b}[0m"), "a b [0m");
        assert_eq!(redact(""), "");
    }
}
