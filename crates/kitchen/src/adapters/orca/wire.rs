//! Orca command lines and JSON response envelopes.
//!
//! Values are always passed as `--flag=value`. Orca's parser treats a
//! separate value that starts with `--` as a new flag, so only the joined form
//! carries arbitrary text such as a brief intact.

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use crate::adapters::orca::{OrcaError, RawOutput, redact};

/// A command line under construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Args(Vec<String>);

impl Args {
    pub(crate) fn command(path: &[&str]) -> Self {
        Self(path.iter().map(|part| (*part).to_owned()).collect())
    }

    pub(crate) fn value(mut self, flag: &str, value: &str) -> Self {
        self.0.push(format!("--{flag}={value}"));
        self
    }

    pub(crate) fn switch(mut self, flag: &str) -> Self {
        self.0.push(format!("--{flag}"));
        self
    }

    pub(crate) fn json(self) -> Vec<String> {
        self.switch("json").0
    }
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
    #[serde(default)]
    message: Option<String>,
}

/// Longest error code kept, in bytes.
const MAX_CODE_BYTES: usize = 64;

fn error_code(code: &str) -> String {
    let valid = !code.is_empty()
        && code.len() <= MAX_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        code.to_owned()
    } else {
        "unrecognized".to_owned()
    }
}

/// Decode an Orca JSON envelope. `ok: false` becomes [`OrcaError::Refused`]
/// with a validated code and a redacted message.
pub(crate) fn result(output: &RawOutput) -> Result<Value, OrcaError> {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&output.stdout) else {
        return Err(if output.stdout.is_empty() {
            OrcaError::NoResult {
                code: output.exit_code,
            }
        } else {
            OrcaError::Malformed { what: "envelope" }
        });
    };
    match (envelope.ok, envelope.result, envelope.error) {
        (true, Some(result), _) => Ok(result),
        (false, _, Some(error)) => Err(OrcaError::Refused {
            code: error_code(&error.code),
            message: redact(error.message.as_deref().unwrap_or_default()),
        }),
        (true, None, _) | (false, _, None) => Err(OrcaError::Malformed { what: "envelope" }),
    }
}

/// Decode a result into `T`.
pub(crate) fn typed<T: DeserializeOwned>(value: Value, what: &'static str) -> Result<T, OrcaError> {
    serde_json::from_value(value).map_err(|_| OrcaError::Malformed { what })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(stdout: &str, exit_code: i32) -> RawOutput {
        RawOutput {
            exit_code: Some(exit_code),
            stdout: stdout.as_bytes().to_vec(),
        }
    }

    #[test]
    fn values_are_joined_to_their_flags() {
        let args = Args::command(&["orchestration", "send"])
            .value("body", "--help me")
            .value("subject", "a b")
            .json();
        assert_eq!(
            args,
            [
                "orchestration",
                "send",
                "--body=--help me",
                "--subject=a b",
                "--json"
            ]
        );
    }

    #[test]
    fn ok_envelope_yields_its_result() {
        let value = result(&output(r#"{"ok":true,"result":{"a":1}}"#, 0));
        assert_eq!(value, Ok(serde_json::json!({"a": 1})));
    }

    #[test]
    fn error_envelope_is_redacted_even_on_success_exit() {
        let failure = result(&output(
            r#"{"ok":false,"error":{"code":"dispatch_not_found","message":"no dcap_secret here"}}"#,
            1,
        ));
        assert_eq!(
            failure,
            Err(OrcaError::Refused {
                code: "dispatch_not_found".to_owned(),
                message: "no [redacted] here".to_owned(),
            })
        );
    }

    #[test]
    fn missing_or_inconsistent_output_is_not_success() {
        assert_eq!(
            result(&output("", 1)),
            Err(OrcaError::NoResult { code: Some(1) })
        );
        assert_eq!(
            result(&output("not json", 0)),
            Err(OrcaError::Malformed { what: "envelope" })
        );
        assert_eq!(
            result(&output(r#"{"ok":true}"#, 0)),
            Err(OrcaError::Malformed { what: "envelope" })
        );
        let odd_code = result(&output(r#"{"ok":false,"error":{"code":"a b"}}"#, 1));
        assert_eq!(
            odd_code,
            Err(OrcaError::Refused {
                code: "unrecognized".to_owned(),
                message: String::new(),
            })
        );
    }
}
