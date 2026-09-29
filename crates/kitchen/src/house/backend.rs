//! The worker backend a house runs its workers on.
//!
//! A [`BackendBinding`] in [`super::HouseConfig`] names the backend kind, the
//! backend namespace house grants name, and the credential that backend acts
//! under. It holds names only, never a credential value. The kind is stored as
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
}

impl BackendKind {
    /// Every backend this Kitchen can build.
    pub const ALL: [Self; 1] = [Self::Orca];

    /// The name a binding stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orca => "orca",
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

/// Which worker backend a house uses. Contains no credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendBinding {
    /// The backend kind, such as `orca`.
    pub kind: BackendName,
    /// Backend namespace; house grants for this backend's effects name it.
    pub backend: BackendId,
    /// The credential the backend acts under, by name. For Orca, the host
    /// session it runs with.
    pub credential: CredentialId,
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
    fn malformed_bindings_are_rejected() {
        for invalid in [
            r#"{"kind":"","backend":"orca","credential":"c"}"#,
            r#"{"kind":"or ca","backend":"orca","credential":"c"}"#,
            r#"{"kind":"orca","backend":"orca"}"#,
            r#"{"kind":"orca","backend":"orca","credential":"c","token":"secret"}"#,
        ] {
            assert!(
                serde_json::from_str::<BackendBinding>(invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
