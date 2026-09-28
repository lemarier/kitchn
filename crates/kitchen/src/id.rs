//! Opaque, case-sensitive identities; these values do not confer authority.

use std::{fmt, str::FromStr};

/// A rejected identifier. Input text is deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentifierError {
    /// Identifiers must contain between one and 64 ASCII bytes.
    #[error("identifier must contain 1 to 64 bytes (received {actual})")]
    Length {
        /// Length of the rejected input in bytes.
        actual: usize,
    },
    /// Identifiers start with an ASCII letter or digit and contain only safe characters.
    #[error(
        "identifier must start with an ASCII letter or digit and contain only ASCII letters, digits, '-' or '_'"
    )]
    Characters,
}

/// Check the common identifier syntax without allocating or normalizing input.
///
/// Accepts 1–64 bytes, starting with an ASCII letter or digit, followed by ASCII
/// letters, digits, hyphens or underscores. Whitespace and Unicode are rejected.
/// Trailing and repeated separators are accepted (for example, `a-`, `a_`, `a--b`).
/// This is not path validation: callers must separately enforce storage ownership.
///
/// # Errors
/// Returns a structured length or character error, without echoing the input.
pub(crate) fn validate_identifier(value: &str) -> Result<(), IdentifierError> {
    if value.is_empty() || value.len() > 64 {
        return Err(IdentifierError::Length {
            actual: value.len(),
        });
    }
    if !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(IdentifierError::Characters);
    }
    Ok(())
}

macro_rules! identifier {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and own an identifier without normalization.
            ///
            /// # Errors
            /// Returns an [`IdentifierError`] without echoing the input.
            pub fn new(value: &str) -> Result<Self, IdentifierError> {
                validate_identifier(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Borrow the validated identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                validate_identifier(&value).map_err(serde::de::Error::custom)?;
                Ok(Self(value))
            }
        }
    };
}

identifier!(
    HouseId,
    "A house identity, distinct from a task identity and from authorization."
);
identifier!(
    TaskId,
    "A Kitchen task identity, independent of backend task handles."
);
identifier!(
    HolderId,
    "A claimant instance (for example one coordinator session), distinct from a backend handle."
);
identifier!(
    BackendId,
    "An execution backend identity. Backend-native handles stay in adapter mappings."
);
identifier!(
    ConsumerId,
    "A workflow consumer scope that must have at most one live owner, such as a pickup loop."
);
identifier!(
    EffectName,
    "The caller's name for one logical external effect within a task attempt."
);
