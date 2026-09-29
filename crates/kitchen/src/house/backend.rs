//! The worker backend a house runs its workers on.
//!
//! A [`BackendBinding`] in [`super::HouseConfig`] names the backend kind, the
//! backend namespace house grants name, and the credential that backend acts
//! under. It holds names only, never a credential value. An HTTP backend's
//! binding also holds its [`HttpEndpoint`]; its bearer token stays in the
//! house's private registry directory. The kind is stored as
//! its name so a house bound to a backend this Kitchen does not know still
//! loads, and is refused by name where a backend is built
//! ([`crate::adapters::resolve_backend`]).

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{BackendId, CredentialId, IdentifierError, id::validate_identifier};

/// A worker backend Kitchen can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendKind {
    /// The Orca desktop orchestrator, through the `orca` CLI.
    Orca,
    /// Any service implementing Kitchen's HTTP worker protocol
    /// ([`crate::adapters::http`]), such as a hosted sandbox control plane.
    Http,
}

impl BackendKind {
    /// Every backend this Kitchen can build.
    pub const ALL: [Self; 2] = [Self::Orca, Self::Http];

    /// The name a binding stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orca => "orca",
            Self::Http => "http",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The stored name of a backend kind: identifier syntax, not necessarily a
/// kind this Kitchen knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BackendName(String);

impl BackendName {
    /// Validate a backend name.
    ///
    /// # Errors
    /// An [`IdentifierError`] for text that is not an identifier.
    pub fn new(value: &str) -> Result<Self, IdentifierError> {
        validate_identifier(value)?;
        Ok(Self(value.to_owned()))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The kind this name denotes, or `None` when this Kitchen has no such
    /// backend.
    #[must_use]
    pub fn kind(&self) -> Option<BackendKind> {
        BackendKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == self.0)
    }
}

impl From<BackendKind> for BackendName {
    fn from(kind: BackendKind) -> Self {
        Self(kind.as_str().to_owned())
    }
}

impl TryFrom<String> for BackendName {
    type Error = IdentifierError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_identifier(&value)?;
        Ok(Self(value))
    }
}

impl From<BackendName> for String {
    fn from(name: BackendName) -> Self {
        name.0
    }
}

impl FromStr for BackendName {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for BackendName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Longest accepted [`HttpEndpoint`], in bytes.
pub const MAX_ENDPOINT_BYTES: usize = 512;

/// A rejected [`HttpEndpoint`]. The input is deliberately excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "an HTTP backend endpoint must be an https URL, or http to a loopback host, of at most 512 bytes, without user information, query, fragment, quotes, or spaces"
)]
pub struct EndpointError;

/// The base URL of an HTTP worker backend, such as
/// `https://sandbox.example.com/kitchen`.
///
/// It must use `https`, except plain `http` to a loopback host
/// (`127.0.0.1`, `localhost`, `[::1]`) for a service on the same machine. It
/// carries no user information, query, or fragment, so no credential can
/// hide in it, and no whitespace, quote, or control character. A trailing
/// slash is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HttpEndpoint(String);

impl HttpEndpoint {
    /// Validate an endpoint.
    ///
    /// # Errors
    /// [`EndpointError`] for an endpoint outside the rules above.
    pub fn new(value: &str) -> Result<Self, EndpointError> {
        let value = value.strip_suffix('/').unwrap_or(value);
        let (secure, rest) = match value.strip_prefix("https://") {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix("http://").ok_or(EndpointError)?),
        };
        let host = Self::host(rest.split('/').next().unwrap_or_default()).ok_or(EndpointError)?;
        if value.len() > MAX_ENDPOINT_BYTES
            || !(secure || matches!(host, "127.0.0.1" | "localhost" | "[::1]"))
            || value.contains(['?', '#', '"', '\\', '\''])
            || value.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(EndpointError);
        }
        Ok(Self(value.to_owned()))
    }

    /// The host of `authority` (`host`, `host:port`, or `[v6]:port`), or
    /// `None` for an empty or malformed host, or a port that is empty, not
    /// decimal, zero, or above 65535.
    fn host(authority: &str) -> Option<&str> {
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']')?;
            let (host, rest) = authority.split_at(end.checked_add(1)?);
            let inner = host.get(1..host.len().checked_sub(1)?)?;
            if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
                return None;
            }
            match rest {
                "" => (host, None),
                _ => (host, Some(rest.strip_prefix(':')?)),
            }
        } else {
            let (host, port) = authority
                .split_once(':')
                .map_or((authority, None), |(host, port)| (host, Some(port)));
            let label = |part: &str| {
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            };
            if !host.split('.').all(label) {
                return None;
            }
            (host, port)
        };
        match port {
            None => Some(host),
            Some(port) if port.bytes().all(|b| b.is_ascii_digit()) => port
                .parse::<u16>()
                .ok()
                .filter(|&port| port != 0)
                .map(|_| host),
            Some(_) => None,
        }
    }

    /// Whether the endpoint uses plain `http`, which only a loopback host may.
    #[must_use]
    pub fn is_plain_http(&self) -> bool {
        self.0.starts_with("http://")
    }

    /// The URL as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HttpEndpoint {
    type Error = EndpointError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<HttpEndpoint> for String {
    fn from(endpoint: HttpEndpoint) -> Self {
        endpoint.0
    }
}

impl fmt::Display for HttpEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which worker backend a house uses. Contains no credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendBinding {
    /// The backend kind, such as `orca`.
    pub kind: BackendName,
    /// Backend namespace; house grants for this backend's effects name it.
    pub backend: BackendId,
    /// The credential the backend acts under, by name. For Orca, the host
    /// session it runs with; for HTTP, the bearer token file
    /// `private/<house>/credentials/<credential>` in the house registry.
    pub credential: CredentialId,
    /// Where an HTTP backend listens. Required for `http`, refused for
    /// every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<HttpEndpoint>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binding_round_trips_and_names_its_kind() -> Result<(), Box<dyn std::error::Error>> {
        let binding: BackendBinding =
            serde_json::from_str(r#"{"kind":"orca","backend":"orca","credential":"orca-host"}"#)?;
        assert_eq!(binding.kind.kind(), Some(BackendKind::Orca));
        assert_eq!(BackendName::from(BackendKind::Orca), binding.kind);
        assert_eq!(
            serde_json::to_string(&binding)?,
            r#"{"kind":"orca","backend":"orca","credential":"orca-host"}"#
        );
        Ok(())
    }

    #[test]
    fn an_unknown_kind_loads_but_is_not_a_kind() -> Result<(), Box<dyn std::error::Error>> {
        let binding: BackendBinding =
            serde_json::from_str(r#"{"kind":"sandbox","backend":"cloud","credential":"token"}"#)?;
        assert_eq!(binding.kind.as_str(), "sandbox");
        assert_eq!(binding.kind.kind(), None);
        // Kinds are case-sensitive, like every other stored identifier.
        assert_eq!(BackendName::new("Orca")?.kind(), None);
        Ok(())
    }

    #[test]
    fn an_http_binding_round_trips_with_its_endpoint() -> Result<(), Box<dyn std::error::Error>> {
        let text = r#"{"kind":"http","backend":"sandbox","credential":"sandbox-token","endpoint":"https://sandbox.example.com/kitchen"}"#;
        let binding: BackendBinding = serde_json::from_str(text)?;
        assert_eq!(binding.kind.kind(), Some(BackendKind::Http));
        assert_eq!(
            binding.endpoint.as_ref().map(HttpEndpoint::as_str),
            Some("https://sandbox.example.com/kitchen")
        );
        assert_eq!(serde_json::to_string(&binding)?, text);
        Ok(())
    }

    #[test]
    fn endpoints_are_https_or_loopback_http_without_hidden_credentials() {
        for accepted in [
            "https://sandbox.example.com",
            "https://sandbox.example.com:8443/kitchen/",
            "http://127.0.0.1:9000",
            "http://localhost/api",
            "http://[::1]:8080",
            "https://sandbox.example.com:65535",
            "https://[2001:db8::1]",
        ] {
            assert!(HttpEndpoint::new(accepted).is_ok(), "{accepted}");
        }
        assert_eq!(
            HttpEndpoint::new("https://example.com/base/").map(String::from),
            Ok("https://example.com/base".to_owned())
        );
        assert!(HttpEndpoint::new("http://127.0.0.1:9000").is_ok_and(|e| e.is_plain_http()));
        for refused in [
            "",
            "https://",
            "http://sandbox.example.com",
            "http://127.0.0.1.example.com",
            "http://localhost:x",
            "https://:443",
            "https://:443/kitchen",
            "https://example.com:",
            "https://example.com:0",
            "https://example.com:65536",
            "https://example.com:+80",
            "https://example.com:80:80",
            "https://exa_mple.com",
            "https://example..com",
            "https://[]:443",
            "https://[::1",
            "https://[::1]x",
            "ftp://example.com",
            "https://user:secret@example.com",
            "https://example.com/?token=secret",
            "https://example.com/#frag",
            "https://exa mple.com",
            "https://example.com/\"x",
            "https://example.com/\n",
        ] {
            assert_eq!(HttpEndpoint::new(refused), Err(EndpointError), "{refused}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_ENDPOINT_BYTES));
        assert_eq!(HttpEndpoint::new(&long), Err(EndpointError));
    }

    #[test]
    fn malformed_bindings_are_rejected() {
        for invalid in [
            r#"{"kind":"","backend":"orca","credential":"c"}"#,
            r#"{"kind":"or ca","backend":"orca","credential":"c"}"#,
            r#"{"kind":"orca","backend":"orca"}"#,
            r#"{"kind":"orca","backend":"orca","credential":"c","token":"secret"}"#,
            r#"{"kind":"http","backend":"b","credential":"c","endpoint":"https://u:p@example.com"}"#,
        ] {
            assert!(
                serde_json::from_str::<BackendBinding>(invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
