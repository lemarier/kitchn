//! One bounded HTTP call through an installed `curl`.
//!
//! The bearer token travels in a curl configuration read from stdin, and the
//! JSON body in a private temporary file the configuration names, so neither
//! appears in the process arguments. The child gets no inherited
//! environment, so no proxy or ambient credential applies.

use std::{fmt, io::Write, path::PathBuf, time::Duration};

use crate::{contracts::IdempotencyKey, house::HttpEndpoint, integrations::github::process};

/// Longest a single call may take, including process start and teardown.
/// It stays below the subprocess runner's own cap.
pub(super) const MAX_CALL: Duration = Duration::from_secs(55);
/// Time the process may take beyond curl's own deadline.
const PROCESS_MARGIN: Duration = Duration::from_secs(2);
/// Largest response body accepted, in bytes.
pub(super) const RESPONSE_LIMIT: usize = 1024 * 1024;

/// The bearer token Kitchen authenticates to the service with. Never printed.
#[derive(Clone)]
pub(super) struct BearerToken(String);

impl BearerToken {
    /// Accept a token of printable ASCII only, so it cannot break the header.
    pub(super) fn new(value: &str) -> Option<Self> {
        let value = value.trim();
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
            .then(|| Self(value.to_owned()))
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([private])")
    }
}

/// An HTTP method the protocol uses.
#[derive(Debug, Clone, Copy)]
pub(super) enum Method {
    Get,
    Post,
}

/// One response: the status and a bounded body.
#[derive(Debug)]
pub(super) struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why a call produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallError {
    /// Refused locally before anything was started or sent.
    NotSent,
    /// The deadline passed; the service may have received the request.
    Timeout,
    /// The connection or process failed, or the response was unreadable or
    /// too large; the service may have received the request.
    Transport,
}

/// Where and as whom calls go.
#[derive(Debug, Clone)]
pub(super) struct Transport {
    pub curl: PathBuf,
    pub endpoint: HttpEndpoint,
    pub token: BearerToken,
}

/// Quote a value for a curl configuration file.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

impl Transport {
    /// Send one request to `path` (below the endpoint, starting with `/`)
    /// and wait at most `timeout` for the whole response.
    pub(super) fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        key: Option<&IdempotencyKey>,
        timeout: Duration,
    ) -> Result<Response, CallError> {
        if timeout.is_zero() || timeout > MAX_CALL || !path.starts_with('/') {
            return Err(CallError::NotSent);
        }
        let private = tempfile::tempdir().map_err(|_| CallError::NotSent)?;
        let mut config = vec![
            format!(
                "url = {}",
                quote(&format!("{}{path}", self.endpoint.as_str()))
            ),
            format!(
                "request = {}",
                quote(match method {
                    Method::Get => "GET",
                    Method::Post => "POST",
                })
            ),
            format!("header = {}", quote("Accept: application/json")),
            format!(
                "header = {}",
                quote(&format!("Authorization: Bearer {}", self.token.0))
            ),
            format!(
                "proto = {}",
                quote(if self.endpoint.is_plain_http() {
                    "=http"
                } else {
                    "=https"
                })
            ),
            "silent".to_owned(),
            format!("max-time = {:.3}", timeout.as_secs_f64()),
            format!("max-filesize = {RESPONSE_LIMIT}"),
            "write-out = \"\\n%{http_code}\"".to_owned(),
        ];
        if let Some(key) = key {
            config.push(format!(
                "header = {}",
                quote(&format!("Idempotency-Key: {key}"))
            ));
        }
        if let Some(body) = body {
            let file = private.path().join("body.json");
            std::fs::File::create(&file)
                .and_then(|mut handle| handle.write_all(body))
                .map_err(|_| CallError::NotSent)?;
            let file = file.to_str().ok_or(CallError::NotSent)?;
            config.push(format!(
                "header = {}",
                quote("Content-Type: application/json")
            ));
            config.push(format!("data-binary = {}", quote(&format!("@{file}"))));
        }
        let mut config = config.join("\n");
        config.push('\n');
        let output = process::run(
            &self.curl,
            &["-q".into(), "--config".into(), "-".into()],
            config.as_bytes(),
            &[],
            timeout.saturating_add(PROCESS_MARGIN),
            RESPONSE_LIMIT.saturating_add(16),
        )
        .map_err(|error| match error {
            crate::integrations::github::IntegrationError::InvalidInput => CallError::NotSent,
            crate::integrations::github::IntegrationError::Timeout => CallError::Timeout,
            _ => CallError::Transport,
        })?;
        match output.code {
            Some(0) => {}
            Some(28) => return Err(CallError::Timeout),
            _ => return Err(CallError::Transport),
        }
        let split = output
            .stdout
            .iter()
            .rposition(|byte| *byte == b'\n')
            .ok_or(CallError::Transport)?;
        let (body, status) = output.stdout.split_at(split);
        let status = std::str::from_utf8(status.get(1..).unwrap_or_default())
            .ok()
            .and_then(|code| code.trim().parse::<u16>().ok())
            .filter(|code| (100..=599).contains(code))
            .ok_or(CallError::Transport)?;
        Ok(Response {
            status,
            body: body.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_values_cannot_add_options() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(
            quote("a\"b\\c\nurl = \"https://evil\""),
            "\"a\\\"b\\\\c\\nurl = \\\"https://evil\\\"\""
        );
    }

    #[test]
    fn tokens_are_printable_ascii_and_never_printed() {
        assert!(BearerToken::new("").is_none());
        assert!(BearerToken::new("a b").is_none());
        assert!(BearerToken::new("tok\nen").is_none());
        let token = BearerToken::new(" secret-token\n");
        assert_eq!(
            token.as_ref().map(|token| token.0.as_str()),
            Some("secret-token")
        );
        assert_eq!(format!("{token:?}"), "Some(BearerToken([private]))");
    }
}
