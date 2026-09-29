//! Read-only inventory of the agent accounts Orca manages.
//!
//! `orca account list --json` reports managed Claude and Codex accounts. That
//! is evidence a family is set up in Orca, not proof Orca can launch it, so
//! callers report it as such. Only the number of accounts is kept: emails and
//! ids in the response are never decoded.

use serde::Deserialize;
use serde_json::Value;

use super::{Invocation, OrcaError, OrcaRunner, wire};
use crate::scheduling::AgentFamily;
use std::time::Duration;

/// Deadline for the account listing.
pub const ACCOUNT_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// The managed accounts Orca reported for each agent family. `None` means
/// Orca's answer did not say, which is different from no accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ManagedAccounts {
    /// Managed Claude accounts.
    pub claude: Option<usize>,
    /// Managed Codex accounts.
    pub codex: Option<usize>,
}

impl ManagedAccounts {
    /// The count Orca reported for `agent`.
    #[must_use]
    pub const fn count(&self, agent: AgentFamily) -> Option<usize> {
        match agent {
            AgentFamily::Claude => self.claude,
            AgentFamily::Codex => self.codex,
        }
    }
}

#[derive(Deserialize)]
struct Family {
    accounts: Vec<serde::de::IgnoredAny>,
}

fn family(result: &Value, key: &str) -> Option<usize> {
    let value = result.get(key)?;
    Family::deserialize(value)
        .ok()
        .map(|family| family.accounts.len())
}

/// Run `orca account list --json` through `runner`.
///
/// # Errors
/// The runner's [`OrcaError`], or [`OrcaError::Malformed`] when the answer is
/// not an Orca envelope. A family missing from a well-formed answer is
/// reported as unknown, not as zero accounts.
pub fn managed_accounts(runner: &dyn OrcaRunner) -> Result<ManagedAccounts, OrcaError> {
    let output = runner.run(&Invocation::new(
        vec!["account".to_owned(), "list".to_owned(), "--json".to_owned()],
        ACCOUNT_LIST_TIMEOUT,
    ))?;
    let result = wire::result(&output)?;
    Ok(ManagedAccounts {
        claude: family(&result, "claude"),
        codex: family(&result, "codex"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::orca::RawOutput;

    struct Canned(Result<RawOutput, OrcaError>);

    impl OrcaRunner for Canned {
        fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError> {
            assert_eq!(invocation.args(), ["account", "list", "--json"]);
            self.0.clone()
        }
    }

    fn says(json: &str) -> Canned {
        Canned(Ok(RawOutput {
            exit_code: Some(0),
            stdout: json.as_bytes().to_vec(),
        }))
    }

    #[test]
    fn counts_accounts_without_reading_their_details() {
        let runner = says(
            r#"{"ok":true,"result":{
                "claude":{"accounts":[{"id":"a","email":"x@y.z"},{"id":"b"}],"activeAccountId":null},
                "codex":{"accounts":[]}}}"#,
        );
        assert_eq!(
            managed_accounts(&runner),
            Ok(ManagedAccounts {
                claude: Some(2),
                codex: Some(0)
            })
        );
    }

    #[test]
    fn a_family_the_answer_omits_or_garbles_is_unknown_not_zero() {
        let runner = says(
            r#"{"ok":true,"result":{"claude":{"accounts":[{}]},"codex":{"accounts":"none"}}}"#,
        );
        let accounts = managed_accounts(&runner);
        assert_eq!(
            accounts,
            Ok(ManagedAccounts {
                claude: Some(1),
                codex: None
            })
        );
        assert_eq!(
            accounts.map(|found| found.count(AgentFamily::Codex)),
            Ok(None)
        );
        assert_eq!(
            managed_accounts(&says(r#"{"ok":true,"result":{}}"#)),
            Ok(ManagedAccounts::default())
        );
    }

    #[test]
    fn failures_are_errors_never_empty_inventories() {
        assert!(matches!(
            managed_accounts(&says("not json")),
            Err(OrcaError::Malformed { .. })
        ));
        assert!(matches!(
            managed_accounts(&says("")),
            Err(OrcaError::NoResult { .. })
        ));
        assert!(matches!(
            managed_accounts(&says(
                r#"{"ok":false,"error":{"code":"nope","message":"no"}}"#
            )),
            Err(OrcaError::Refused { .. })
        ));
        assert_eq!(
            managed_accounts(&Canned(Err(OrcaError::Timeout))),
            Err(OrcaError::Timeout)
        );
    }
}
