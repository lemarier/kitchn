//! Structured failures shared by Kitchen library callers.

/// A rejected external identifier. Input text is deliberately excluded from errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Identifiers must contain between one and 64 ASCII bytes.
    #[error("identifier must contain 1 to 64 bytes (received {actual})")]
    IdentifierLength {
        /// Length of the rejected input in bytes.
        actual: usize,
    },
    /// Identifiers start with an ASCII letter or digit and contain only safe characters.
    #[error(
        "identifier must start with an ASCII letter or digit and contain only ASCII letters, digits, '-' or '_'"
    )]
    IdentifierCharacters,
}
