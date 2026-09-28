//! Orca runtime probing, version support, and declared capabilities.

use std::{fmt, time::Duration};

use serde::Deserialize;

use crate::{
    adapters::orca::{Invocation, OrcaError, OrcaRunner, redact, wire},
    contracts::{Capability, CapabilitySet, Support},
};

/// The Orca versions this adapter supports, as a range.
pub const SUPPORTED_VERSIONS: &str = ">=1.4.212, <1.5.0";

/// The oldest supported version, the one the adapter was verified against.
const MIN_VERSION: OrcaVersion = OrcaVersion {
    major: 1,
    minor: 4,
    patch: 212,
};

/// Runtime features every Orca call path of this adapter relies on.
pub const REQUIRED_FEATURES: [&str; 2] = [
    "orchestration.contract.v1",
    "orchestration.worker-stop-verdict.v1",
];

/// A parsed `major.minor.patch` Orca version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrcaVersion {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version.
    pub patch: u32,
}

impl OrcaVersion {
    /// Parse `major.minor.patch`, ignoring a `-prerelease` or `+build` suffix.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let core = value.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(str::parse::<u32>);
        let version = Self {
            major: parts.next()?.ok()?,
            minor: parts.next()?.ok()?,
            patch: parts.next()?.ok()?,
        };
        parts.next().is_none().then_some(version)
    }

    /// Whether this adapter supports the version.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.major == MIN_VERSION.major
            && self.minor == MIN_VERSION.minor
            && self.patch >= MIN_VERSION.patch
    }
}

impl fmt::Display for OrcaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What `orca status --json` established about the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// The runtime's version.
    pub version: OrcaVersion,
    /// Advertised runtime features.
    pub features: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    runtime: StatusRuntime,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusRuntime {
    state: String,
    reachable: bool,
    app_version: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Probe the runtime and check that the adapter supports it.
///
/// # Errors
/// [`OrcaError::RuntimeNotReady`] when the runtime is down,
/// [`OrcaError::UnsupportedVersion`] outside [`SUPPORTED_VERSIONS`],
/// [`OrcaError::MissingRuntimeFeature`] when a [`REQUIRED_FEATURES`] entry is
/// not advertised, and call or parse failures otherwise.
pub fn probe(runner: &dyn OrcaRunner, deadline: Duration) -> Result<RuntimeInfo, OrcaError> {
    let args = wire::Args::command(&["status"]).json();
    let output = runner.run(&Invocation::new(args, deadline))?;
    let status: Status = wire::typed(wire::result(&output)?, "status")?;
    if !status.runtime.reachable || status.runtime.state != "ready" {
        return Err(OrcaError::RuntimeNotReady);
    }
    let Some(version) = OrcaVersion::parse(&status.runtime.app_version) else {
        return Err(OrcaError::UnsupportedVersion {
            found: redact(&status.runtime.app_version),
            supported: SUPPORTED_VERSIONS,
        });
    };
    if !version.is_supported() {
        return Err(OrcaError::UnsupportedVersion {
            found: version.to_string(),
            supported: SUPPORTED_VERSIONS,
        });
    }
    if let Some(missing) = REQUIRED_FEATURES.iter().find(|feature| {
        !status
            .runtime
            .capabilities
            .iter()
            .any(|have| have == *feature)
    }) {
        return Err(OrcaError::MissingRuntimeFeature(missing));
    }
    Ok(RuntimeInfo {
        version,
        features: status.runtime.capabilities,
    })
}

/// The capabilities this adapter declares for a supported runtime.
///
/// Full support is declared only where the adapter implements the path and
/// Orca documents the behavior; partial where Orca has known gaps. Absent
/// entries (single consumer, run timeout, house credentials, console close)
/// are unsupported and must be provided by Kitchen or refused.
#[must_use]
pub fn capabilities() -> CapabilitySet {
    use Capability as C;
    CapabilitySet::supporting([
        // A Task titled with the launch key, dispatched with `worker-start`.
        C::WorkerLaunchIsolated,
        // Ready only when the worker reached `ready` and Orca's fleet
        // liveness is `live`; start acceptance alone is `Starting`.
        C::WorkerLaunchReadiness,
        // `send --to dispatch:`, `reply`, and the Run mailbox.
        C::WorkerMessaging,
        // `worker-show` projection; settled only on an accepted report or stop.
        C::WorkerStatusAndOutcome,
        // `worker-stop` fences the Dispatch and stops only its agent terminal.
        C::WorkerCancel,
        // `worker-release`: idempotent, retains anything it cannot prove owned.
        C::ResourceRelease,
        // `worker-list` with fleet liveness and the launch key as owner.
        // Dirty or unpushed work is not reported; cleanup reads Git for that.
        C::ResourceInventory,
        // Automations create, list, edit, remove, run; installs reconcile by
        // consumer name and are always created disabled.
        C::ScheduleManage,
        // `--provider` for schedules and `--agent` for workers.
        C::AgentSelectFamily,
        // `--reuse-session` for existing-workspace schedules.
        C::SessionReuse,
    ])
    // Lookup and same-key idempotency are declared per effect kind, and only
    // where the adapter can prove them from Orca's own records: a launch
    // through its Task, a cancel and a release through the Dispatch's state
    // (a repeat is answered from that record), and a schedule install, state
    // change, and removal through the automation listing. Messages and
    // replies carry no key Orca records, and a trial starts a new run each
    // time, so those kinds declare neither.
    .with(C::LookupLaunchWorker, Support::Supported)
    .with(C::IdempotentLaunchWorker, Support::Supported)
    .with(C::LookupCancelWorker, Support::Supported)
    .with(C::IdempotentCancelWorker, Support::Supported)
    .with(C::LookupReleaseResource, Support::Supported)
    .with(C::IdempotentReleaseResource, Support::Supported)
    .with(C::LookupInstallDisabledSchedule, Support::Supported)
    .with(C::IdempotentInstallDisabledSchedule, Support::Supported)
    .with(C::LookupSetScheduleState, Support::Supported)
    .with(C::IdempotentSetScheduleState, Support::Supported)
    .with(C::LookupRemoveSchedule, Support::Supported)
    .with(C::IdempotentRemoveSchedule, Support::Supported)
    // A non-zero precheck exit is recorded as a skip: idle and error look alike.
    .with(C::SchedulePrecheck, Support::Partial)
    // `run-use` binds a terminal but records no relinquish; Kitchen owns the checkpoint.
    .with(C::RunTransfer, Support::Partial)
    // Partial: automations cannot select a model. Worker launches can, and
    // declare it through the descriptor's `worker_selection`, which the
    // state store checks before every launch that names a selection.
    .with(C::AgentSelectModel, Support::Partial)
    // Automation run usage is mostly reported as unavailable.
    .with(C::UsageAttribution, Support::Partial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_bound_support() {
        let tested = OrcaVersion::parse("1.4.212");
        assert_eq!(tested, Some(MIN_VERSION));
        assert!(OrcaVersion::parse("1.4.300-beta.1").is_some_and(OrcaVersion::is_supported));
        assert!(!OrcaVersion::parse("1.4.211").is_some_and(OrcaVersion::is_supported));
        assert!(!OrcaVersion::parse("1.5.0").is_some_and(OrcaVersion::is_supported));
        assert!(!OrcaVersion::parse("2.4.212").is_some_and(OrcaVersion::is_supported));
        for invalid in ["", "1.4", "1.4.x", "1.4.212.1", "v1.4.212"] {
            assert_eq!(OrcaVersion::parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn unsupported_capabilities_stay_undeclared() {
        let declared = capabilities();
        for capability in [
            Capability::ScheduleSingleConsumer,
            Capability::ScheduleRunTimeout,
            Capability::HouseCredentials,
            Capability::ResourceCloseConsole,
        ] {
            assert_eq!(declared.support(capability), None, "{capability}");
        }
        assert_eq!(
            declared.support(Capability::SchedulePrecheck),
            Some(Support::Partial)
        );
        // The all-kinds shorthands stay undeclared: messages, replies, and
        // trials cannot be looked up or deduplicated by key.
        for shorthand in [
            Capability::EffectLookup,
            Capability::EffectIdempotentRequests,
            Capability::LookupMessageWorker,
            Capability::IdempotentMessageWorker,
            Capability::LookupReplyToWorker,
            Capability::IdempotentReplyToWorker,
            Capability::LookupTrialSchedule,
            Capability::IdempotentTrialSchedule,
        ] {
            assert_eq!(declared.support(shorthand), None, "{shorthand}");
        }
        for declared_kind in [
            Capability::LookupLaunchWorker,
            Capability::IdempotentLaunchWorker,
            Capability::LookupCancelWorker,
            Capability::IdempotentCancelWorker,
            Capability::LookupReleaseResource,
            Capability::IdempotentReleaseResource,
        ] {
            assert!(declared.supports(declared_kind), "{declared_kind}");
        }
        assert!(declared.supports(Capability::ResourceInventory));
    }
}
