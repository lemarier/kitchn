//! Opaque, case-sensitive identities; these values do not confer authority.

use std::{fmt, str::FromStr};

use crate::Error;

/// Check the common identifier syntax without allocating or normalizing input.
///
/// Accepts 1–64 bytes, starting with an ASCII letter or digit, followed by ASCII
/// letters, digits, hyphens or underscores. Whitespace and Unicode are rejected.
/// This is not path validation: callers must separately enforce storage ownership.
///
/// # Errors
/// Returns a structured length or character error, without echoing the input.
pub fn validate_identifier(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 64 {
        return Err(Error::IdentifierLength {
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
        return Err(Error::IdentifierCharacters);
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
            /// Returns an error when [`validate_identifier`] rejects the input.
            pub fn new(value: &str) -> Result<Self, Error> {
                validate_identifier(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Borrow the validated identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
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
