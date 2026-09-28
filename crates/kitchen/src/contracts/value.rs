//! Validated scalar values shared by contracts and persisted state.

use std::{
    fmt,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::contracts::ContractError;

/// The kind of value a validation error refers to. Rejected input is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValueKind {
    /// An opaque backend handle or link.
    ExternalRef,
    /// A Git object identifier.
    CommitId,
    /// An `owner/name` repository.
    Repository,
    /// Bounded free text such as a worker brief.
    Text,
    /// A capability name.
    Capability,
    /// A lease duration.
    LeaseTtl,
    /// A retry policy bound.
    RetryPolicy,
    /// A role name.
    Role,
    /// A permission name.
    Permission,
    /// A backend receipt.
    Receipt,
    /// An effect kind name.
    EffectKind,
    /// A Git branch name.
    BranchName,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExternalRef => "external reference",
            Self::CommitId => "commit id",
            Self::Repository => "repository",
            Self::Text => "text",
            Self::Capability => "capability",
            Self::LeaseTtl => "lease duration",
            Self::RetryPolicy => "retry policy",
            Self::Role => "role",
            Self::Permission => "permission",
            Self::Receipt => "receipt",
            Self::EffectKind => "effect kind",
            Self::BranchName => "branch name",
        })
    }
}

/// Milliseconds since the Unix epoch. Callers inject time so decisions are testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Build a timestamp from Unix milliseconds.
    #[must_use]
    pub const fn from_unix_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Unix milliseconds.
    #[must_use]
    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    /// Add a duration, saturating at the maximum representable time.
    #[must_use]
    pub fn saturating_add(self, duration: Duration) -> Self {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(millis))
    }

    /// Time elapsed since `earlier`, or zero when `earlier` is later.
    #[must_use]
    pub const fn saturating_since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}ms", self.0)
    }
}

/// A source of the current time.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Timestamp;
}

/// The host wall clock. A clock before the Unix epoch reads as zero.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        Timestamp(millis)
    }
}

/// Maximum length of an [`ExternalRef`] in bytes.
pub const MAX_EXTERNAL_REF_BYTES: usize = 256;
/// Maximum length of [`Text`] in bytes.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

fn invalid(kind: ValueKind) -> ContractError {
    ContractError::InvalidValue { kind }
}

fn validate_external_ref(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > MAX_EXTERNAL_REF_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid(ValueKind::ExternalRef));
    }
    Ok(())
}

fn validate_commit_id(value: &str) -> Result<(), ContractError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(ValueKind::CommitId));
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<(), ContractError> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err(invalid(ValueKind::Repository));
    };
    let owner_valid = (1..=39).contains(&owner.len())
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let name_valid = (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if owner_valid && name_valid {
        Ok(())
    } else {
        Err(invalid(ValueKind::Repository))
    }
}

/// Maximum length of a [`BranchName`] in bytes.
pub const MAX_BRANCH_NAME_BYTES: usize = 255;

/// A branch name Git accepts (`git check-ref-format --branch`), restricted
/// to printable ASCII without spaces.
fn validate_branch_name(value: &str) -> Result<(), ContractError> {
    let forbidden = |byte: u8| {
        !byte.is_ascii_graphic() || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    };
    let valid = !value.is_empty()
        && value.len() <= MAX_BRANCH_NAME_BYTES
        && !value.bytes().any(forbidden)
        && !value.starts_with('-')
        && value != "@"
        && !value.contains("..")
        && !value.contains("@{")
        && !value.ends_with('.')
        && value.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        });
    if valid {
        Ok(())
    } else {
        Err(invalid(ValueKind::BranchName))
    }
}

fn validate_text(value: &str) -> Result<(), ContractError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        return Err(invalid(ValueKind::Text));
    }
    Ok(())
}

macro_rules! validated_string {
    ($name:ident, $validate:path, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and own the value without normalization.
            ///
            /// # Errors
            /// Returns [`ContractError::InvalidValue`] without echoing the input.
            pub fn new(value: &str) -> Result<Self, ContractError> {
                $validate(value)?;
                Ok(Self(value.to_owned()))
            }

            /// Borrow the validated value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ContractError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                $validate(&value).map_err(serde::de::Error::custom)?;
                Ok(Self(value))
            }
        }
    };
}

macro_rules! display_value {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

validated_string!(
    ExternalRef,
    validate_external_ref,
    "An opaque backend handle, receipt, or link: 1–256 printable ASCII bytes without spaces.\n\nKitchen never interprets its content; adapters map it to native identifiers."
);
display_value!(ExternalRef);

validated_string!(
    CommitId,
    validate_commit_id,
    "A full lowercase hexadecimal Git object id (SHA-1 or SHA-256)."
);
display_value!(CommitId);

validated_string!(
    BranchName,
    validate_branch_name,
    "A Git branch name, such as `lemarier/core-contracts`, validated like `git check-ref-format --branch` and limited to printable ASCII."
);
display_value!(BranchName);

validated_string!(
    Repository,
    validate_repository,
    "A repository written as `owner/name`."
);
display_value!(Repository);

impl Repository {
    /// The owner segment.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.split_once('/').map_or("", |(owner, _)| owner)
    }

    /// The name segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, name)| name)
    }
}

validated_string!(
    Text,
    validate_text,
    "Bounded free text (1 byte to 64 KiB, no NUL), such as a worker brief.\n\nIts `Debug` output shows only the length so private context does not reach logs."
);

impl fmt::Debug for Text {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Text({} bytes)", self.0.len())
    }
}

/// How long a claim or consumer lease stays live without renewal (1 second to 24 hours).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseTtl(Duration);

impl LeaseTtl {
    /// Shortest accepted lease.
    pub const MIN: Duration = Duration::from_secs(1);
    /// Longest accepted lease.
    pub const MAX: Duration = Duration::from_secs(24 * 60 * 60);

    /// Validate a lease duration.
    ///
    /// # Errors
    /// Returns [`ContractError::InvalidValue`] outside `MIN..=MAX`.
    pub fn new(duration: Duration) -> Result<Self, ContractError> {
        if duration < Self::MIN || duration > Self::MAX {
            return Err(invalid(ValueKind::LeaseTtl));
        }
        Ok(Self(duration))
    }

    /// The validated duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }
}
