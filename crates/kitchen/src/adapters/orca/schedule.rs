//! Schedules as Orca automations.
//!
//! Kitchen names each automation `kitchen:<house>:<consumer>`, after the
//! workflow consumer scope it serves, and acts only on
//! automations whose name decodes for its own house, so existing automations
//! are never touched. Orca's automation commands take no request key, so:
//!
//! - an install first reconciles against a complete listing with
//!   [`plan_install`] and reuses an existing schedule;
//! - a create whose response is lost is resolved by listing again, and stays
//!   [`OrcaError::InstallUncertain`] when the listing cannot show it;
//! - every change is read back before it is reported as done.
//!
//! Installs are always created disabled. Enabling is a separate call.

use serde::Deserialize;
use serde_json::Value;

use crate::{
    ConsumerId, HouseId,
    adapters::orca::{OrcaBackend, OrcaError, OrcaRunner, backend, wire},
    contracts::{
        EffectFailure, ExternalRef, Lookup, NotAppliedReason, Receipt, ResourceKind, ResourceRef,
        ScheduleEffect, Timestamp, UncertainReason,
    },
    scheduling::{
        InstallPlan, InstalledSchedule, MAX_SCHEDULE_RUNS, ObservedScheduleState, PrecheckOutcome,
        Recurrence, RunOutcome, ScheduleObservation, ScheduleRun, ScheduleSpec, ScheduleState,
        ScheduleWorkspace, plan_install,
    },
};

/// Most automations one listing may hold before it is refused as incomplete.
pub const MAX_AUTOMATIONS: usize = 500;

const NAME_PREFIX: &str = "kitchen";

/// The Orca automation name Kitchen uses for `consumer` in `house`.
#[must_use]
pub fn native_schedule_name(house: &HouseId, consumer: &ConsumerId) -> String {
    format!("{NAME_PREFIX}:{house}:{consumer}")
}

fn decode_name(house: &HouseId, native: &str) -> Option<ConsumerId> {
    let rest = native
        .strip_prefix(NAME_PREFIX)?
        .strip_prefix(':')?
        .strip_prefix(house.as_str())?
        .strip_prefix(':')?;
    ConsumerId::new(rest).ok()
}

/// Quote an argument vector for the POSIX shell Orca runs prechecks in on
/// macOS and Linux hosts. Windows hosts are not supported by this adapter.
fn shell_command(argv: &[crate::contracts::Text]) -> String {
    argv.iter()
        .map(|arg| format!("'{}'", arg.as_str().replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
struct AutomationList {
    automations: Vec<Automation>,
}

#[derive(Deserialize)]
struct Automation {
    id: String,
    name: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct Created {
    #[serde(default)]
    automation: Option<Automation>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct RunList {
    runs: Vec<WireRun>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRun {
    status: String,
    #[serde(default)]
    scheduled_for: Option<u64>,
    #[serde(default)]
    precheck_result: Option<WirePrecheck>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePrecheck {
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    error: Option<Value>,
}

/// Orca stores any non-zero precheck exit as `skipped_precheck`; its
/// recorded result tells idle (exit 1) from an error (timeout, spawn error,
/// or any other code), following [`PrecheckOutcome`].
fn run_outcome(run: &WireRun) -> RunOutcome {
    match run.status.as_str() {
        "pending" | "running" | "dispatching" => RunOutcome::Pending,
        // `completed` means the launch step finished, not that the agent started.
        "completed" | "dispatched" => RunOutcome::LaunchReported,
        "skipped_precheck" => match &run.precheck_result {
            Some(precheck) => {
                let failed = precheck.timed_out
                    || precheck
                        .error
                        .as_ref()
                        .is_some_and(|error| !error.is_null());
                match (failed, PrecheckOutcome::from_exit_code(precheck.exit_code)) {
                    (false, PrecheckOutcome::Idle) => RunOutcome::PrecheckIdle,
                    // Orca skipped the run, so exit 0 here is inconsistent.
                    (false, PrecheckOutcome::Actionable) => RunOutcome::Unknown,
                    (true, _) | (false, PrecheckOutcome::Error) => RunOutcome::PrecheckFailed,
                }
            }
            None => RunOutcome::Unknown,
        },
        "skipped_missed" | "skipped_unavailable" | "skipped_needs_interactive_auth" => {
            RunOutcome::Skipped
        }
        "dispatch_failed" => RunOutcome::LaunchFailed,
        _ => RunOutcome::Unknown,
    }
}

const fn state_of(enabled: bool) -> ObservedScheduleState {
    if enabled {
        ObservedScheduleState::Active
    } else {
        ObservedScheduleState::Paused
    }
}

impl<R: OrcaRunner> OrcaBackend<R> {
    fn schedule_ref(&self, id: &str) -> Result<ResourceRef, OrcaError> {
        Ok(ResourceRef {
            kind: ResourceKind::Schedule,
            backend: self.config().backend.clone(),
            handle: ExternalRef::new(id)
                .map_err(|_| OrcaError::Malformed { what: "automation" })?,
        })
    }

    fn automations(&self) -> Result<Vec<Automation>, OrcaError> {
        let args = wire::Args::command(&["automations", "list"]).json();
        let list: AutomationList = wire::typed(
            self.call(args, self.config().call_timeout)?,
            "automation list",
        )?;
        if list.automations.len() > MAX_AUTOMATIONS {
            return Err(OrcaError::ListingTooLong {
                limit: MAX_AUTOMATIONS,
            });
        }
        Ok(list.automations)
    }

    /// Every Kitchen schedule installed for this house.
    ///
    /// # Errors
    /// Call and parse failures, and [`OrcaError::ListingTooLong`].
    pub fn installed_schedules(&self) -> Result<Vec<InstalledSchedule>, OrcaError> {
        let house = &self.config().house;
        self.automations()?
            .into_iter()
            .filter_map(|automation| {
                decode_name(house, &automation.name).map(|consumer| {
                    Ok(InstalledSchedule {
                        resource: self.schedule_ref(&automation.id)?,
                        consumer,
                        state: state_of(automation.enabled),
                    })
                })
            })
            .collect()
    }

    /// Find `schedule` and confirm Kitchen installed it for this house.
    fn owned(&self, schedule: &ResourceRef) -> Result<Automation, OrcaError> {
        if schedule.kind != ResourceKind::Schedule || schedule.backend != self.config().backend {
            return Err(OrcaError::NotKitchenOwned);
        }
        let automation = self
            .automations()?
            .into_iter()
            .find(|automation| automation.id == schedule.handle.as_str())
            .ok_or(OrcaError::ScheduleNotFound)?;
        if decode_name(&self.config().house, &automation.name).is_none() {
            return Err(OrcaError::NotKitchenOwned);
        }
        Ok(automation)
    }

    fn create_args(&self, spec: &ScheduleSpec) -> Result<Vec<String>, OrcaError> {
        let config = self.config();
        let mut args = wire::Args::command(&["automations", "create"])
            .value(
                "name",
                &native_schedule_name(&config.house, spec.consumer()),
            )
            .value("prompt", spec.prompt().as_str())
            .value("provider", spec.agent().as_str())
            .value("timezone", spec.timezone().as_str())
            .value(
                "missed-run-grace-minutes",
                &spec.missed_run_grace().get().to_string(),
            )
            .switch("disabled");
        args = match spec.recurrence() {
            Recurrence::Hourly => args.value("trigger", "hourly"),
            Recurrence::Daily(at) => args
                .value("trigger", "daily")
                .value("time", &at.to_string()),
            Recurrence::Weekdays(at) => args
                .value("trigger", "weekdays")
                .value("time", &at.to_string()),
            Recurrence::Weekly(day, at) => args
                .value("trigger", "weekly")
                .value("day", &day.number().to_string())
                .value("time", &at.to_string()),
            Recurrence::Cron(expr) => args.value("trigger", expr.as_str()),
        };
        if let Some(precheck) = spec.precheck() {
            args = args
                .value("precheck", &shell_command(precheck.argv()))
                .value(
                    "precheck-timeout",
                    &precheck.timeout().whole_seconds().to_string(),
                );
        }
        args = match spec.workspace() {
            ScheduleWorkspace::NewPerRun => {
                let args = args
                    .value("workspace-mode", "new-per-run")
                    .value("repo", config.repo.as_str());
                match &config.base_branch {
                    Some(base) => args.value("base-branch", base.as_str()),
                    None => args,
                }
            }
            ScheduleWorkspace::Existing(workspace) => {
                if workspace.kind != ResourceKind::Worktree || workspace.backend != config.backend {
                    return Err(OrcaError::NotKitchenOwned);
                }
                args.value("workspace-mode", "existing")
                    .value("workspace", &format!("id:{}", workspace.handle))
            }
        };
        args = if spec.reuse_session() {
            args.switch("reuse-session")
        } else {
            args.switch("fresh-session")
        };
        Ok(args.json())
    }

    /// Install `spec` paused, or return the schedule already installed for
    /// its consumer.
    ///
    /// # Errors
    /// [`OrcaError::DuplicateSchedules`] when several share the name,
    /// [`OrcaError::InstallUncertain`] when a create may have happened but no
    /// listing shows it, [`OrcaError::StateMismatch`] when a new schedule does
    /// not read back paused, and call or parse failures.
    pub fn install_schedule(&self, spec: &ScheduleSpec) -> Result<ResourceRef, OrcaError> {
        let consumer = spec.consumer();
        match plan_install(consumer, &self.installed_schedules()?) {
            InstallPlan::Installed(existing) => return Ok(existing),
            InstallPlan::Duplicates(duplicates) => {
                return Err(OrcaError::DuplicateSchedules {
                    count: duplicates.len(),
                });
            }
            InstallPlan::Create => {}
        }
        let args = self.create_args(spec)?;
        let created = match self.call(args, self.config().call_timeout) {
            Ok(value) => wire::typed::<Created>(value, "automation create")
                .ok()
                .and_then(|created| {
                    created
                        .automation
                        .map(|automation| automation.id)
                        .or(created.id)
                }),
            Err(OrcaError::Spawn(kind)) => return Err(OrcaError::Spawn(kind)),
            Err(_) => None,
        };
        // Resolve the result from the listing, whether or not the response
        // named an id: it is the only view that also shows duplicates.
        let installed = self
            .installed_schedules()
            .map_err(|_| OrcaError::InstallUncertain)?;
        let resource = match plan_install(consumer, &installed) {
            InstallPlan::Installed(resource) => resource,
            InstallPlan::Duplicates(duplicates) => {
                return Err(OrcaError::DuplicateSchedules {
                    count: duplicates.len(),
                });
            }
            InstallPlan::Create => return Err(OrcaError::InstallUncertain),
        };
        if created.is_some_and(|id| id != resource.handle.as_str()) {
            return Err(OrcaError::StateMismatch);
        }
        let paused = installed.iter().any(|schedule| {
            schedule.resource == resource && schedule.state == ObservedScheduleState::Paused
        });
        if paused {
            Ok(resource)
        } else {
            Err(OrcaError::StateMismatch)
        }
    }

    /// Observe a schedule's state and up to [`MAX_SCHEDULE_RUNS`] recent runs.
    ///
    /// # Errors
    /// Call and parse failures. A schedule absent from a complete listing is
    /// reported as [`ObservedScheduleState::Missing`], not an error.
    pub fn inspect_schedule(
        &self,
        schedule: &ResourceRef,
    ) -> Result<ScheduleObservation, OrcaError> {
        let automation = match self.owned(schedule) {
            Ok(automation) => automation,
            Err(OrcaError::ScheduleNotFound) => {
                return Ok(ScheduleObservation {
                    state: ObservedScheduleState::Missing,
                    recent_runs: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let args = wire::Args::command(&["automations", "runs"])
            .value("id", &automation.id)
            .json();
        let mut runs: RunList = wire::typed(
            self.call(args, self.config().call_timeout)?,
            "automation runs",
        )?;
        runs.runs
            .sort_by_key(|run| std::cmp::Reverse(run.scheduled_for));
        Ok(ScheduleObservation {
            state: state_of(automation.enabled),
            recent_runs: runs
                .runs
                .iter()
                .take(MAX_SCHEDULE_RUNS)
                .map(|run| ScheduleRun {
                    outcome: run_outcome(run),
                    scheduled_for: run.scheduled_for.map(Timestamp::from_unix_millis),
                })
                .collect(),
        })
    }

    /// Pause or activate a Kitchen schedule and read the state back.
    ///
    /// Activation starts a live consumer; the caller must hold that authority.
    ///
    /// # Errors
    /// [`OrcaError::NotKitchenOwned`], [`OrcaError::ScheduleNotFound`],
    /// [`OrcaError::StateMismatch`] when the read-back differs, and call failures.
    pub fn set_schedule_state(
        &self,
        schedule: &ResourceRef,
        state: ScheduleState,
    ) -> Result<(), OrcaError> {
        let automation = self.owned(schedule)?;
        let switch = match state {
            ScheduleState::Paused => "disabled",
            ScheduleState::Active => "enabled",
        };
        let args = wire::Args::command(&["automations", "edit"])
            .value("id", &automation.id)
            .switch(switch)
            .json();
        let outcome = self.call(args, self.config().call_timeout);
        let now = self.owned(schedule)?;
        let expected = matches!(state, ScheduleState::Active);
        match outcome {
            _ if now.enabled == expected => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(OrcaError::StateMismatch),
        }
    }

    /// Remove a Kitchen schedule and its run history, and confirm it is gone.
    /// A schedule already absent from a complete listing counts as removed.
    ///
    /// # Errors
    /// [`OrcaError::NotKitchenOwned`], [`OrcaError::StateMismatch`] when it
    /// is still listed, and call failures.
    pub fn remove_schedule(&self, schedule: &ResourceRef) -> Result<(), OrcaError> {
        let automation = match self.owned(schedule) {
            Ok(automation) => automation,
            Err(OrcaError::ScheduleNotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let args = wire::Args::command(&["automations", "remove"])
            .value("id", &automation.id)
            .json();
        let outcome = self.call(args, self.config().call_timeout);
        match (self.owned(schedule), outcome) {
            (Err(OrcaError::ScheduleNotFound), _) => Ok(()),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(_), Ok(_)) => Err(OrcaError::StateMismatch),
        }
    }

    /// Run a paused Kitchen schedule once now, without enabling it.
    ///
    /// Orca's `automations run` takes no request key: a lost response must be
    /// resolved with [`Self::inspect_schedule`] before another trial.
    ///
    /// # Errors
    /// [`OrcaError::TrialRequiresPaused`] for an active schedule, ownership
    /// errors, and call failures.
    pub fn trial_schedule(&self, schedule: &ResourceRef) -> Result<(), OrcaError> {
        let automation = self.owned(schedule)?;
        if automation.enabled {
            return Err(OrcaError::TrialRequiresPaused);
        }
        let args = wire::Args::command(&["automations", "run"])
            .value("id", &automation.id)
            .json();
        let _: Value = self.call(args, self.config().call_timeout)?;
        Ok(())
    }
}

/// Map a schedule call failure onto the effect contract. Refusals checked
/// before any change prove nothing happened; anything after a change was
/// sent is uncertain.
fn schedule_failure(error: &OrcaError) -> EffectFailure {
    match error {
        OrcaError::Spawn(_)
        | OrcaError::NotKitchenOwned
        | OrcaError::ScheduleNotFound
        | OrcaError::DuplicateSchedules { .. }
        | OrcaError::BranchMismatch { .. }
        | OrcaError::TrialRequiresPaused
        | OrcaError::Schedule(_)
        | OrcaError::Contract(_) => EffectFailure::NotApplied(NotAppliedReason::Rejected),
        OrcaError::Timeout => EffectFailure::Uncertain(UncertainReason::Timeout),
        OrcaError::Io(_) => EffectFailure::Uncertain(UncertainReason::Transport),
        OrcaError::OutputLimit { .. }
        | OrcaError::NoResult { .. }
        | OrcaError::Malformed { .. }
        | OrcaError::Refused { .. }
        | OrcaError::RuntimeNotReady
        | OrcaError::UnsupportedVersion { .. }
        | OrcaError::MissingRuntimeFeature(_)
        | OrcaError::ListingTooLong { .. }
        | OrcaError::InstallUncertain
        | OrcaError::StateMismatch => EffectFailure::Uncertain(UncertainReason::ResponseLost),
    }
}

/// A receipt referenced by the automation id. An install created the
/// schedule; every other change touches an existing one.
fn schedule_receipt(schedule: &ResourceRef, created: bool) -> Result<Receipt, EffectFailure> {
    let (created, touched) = if created {
        (vec![schedule.clone()], Vec::new())
    } else {
        (Vec::new(), vec![schedule.clone()])
    };
    Receipt::new(schedule.handle.clone(), created, touched)
        .map_err(|_| EffectFailure::Uncertain(UncertainReason::ResponseLost))
}

impl<R: OrcaRunner> OrcaBackend<R> {
    /// Perform one schedule effect. Receipts are referenced by the
    /// automation id, so a lookup derives the same receipt.
    pub(crate) fn execute_schedule(
        &self,
        effect: &ScheduleEffect,
    ) -> Result<Receipt, EffectFailure> {
        match effect {
            ScheduleEffect::InstallDisabled { schedule } => {
                let installed = self
                    .install_schedule(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(&installed, true)
            }
            ScheduleEffect::SetState { schedule, state } => {
                self.set_schedule_state(schedule, *state)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, false)
            }
            ScheduleEffect::Remove { schedule } => {
                self.remove_schedule(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, false)
            }
            ScheduleEffect::Trial { schedule } => {
                self.trial_schedule(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, false)
            }
        }
    }

    /// Look up a schedule effect from the installed inventory. A trial
    /// leaves no record Kitchen can match, so it stays unknown.
    pub(crate) fn resolve_schedule(
        &self,
        effect: &ScheduleEffect,
    ) -> Result<Lookup, crate::contracts::BackendUnavailable> {
        let unavailable = |error: OrcaError| backend::read_failure(&error);
        let applied = |schedule: &ResourceRef, created: bool| {
            schedule_receipt(schedule, created)
                .map(Lookup::Applied)
                .map_err(|_| crate::contracts::BackendUnavailable::Transport)
        };
        match effect {
            ScheduleEffect::InstallDisabled { schedule } => {
                let installed = self.installed_schedules().map_err(unavailable)?;
                match plan_install(schedule.consumer(), &installed) {
                    InstallPlan::Installed(schedule) => applied(&schedule, true),
                    InstallPlan::Create | InstallPlan::Duplicates(_) => Ok(Lookup::Unknown),
                }
            }
            ScheduleEffect::SetState { schedule, state } => match self.owned(schedule) {
                Ok(automation) if automation.enabled == matches!(state, ScheduleState::Active) => {
                    applied(schedule, false)
                }
                Ok(_) | Err(OrcaError::ScheduleNotFound | OrcaError::NotKitchenOwned) => {
                    Ok(Lookup::Unknown)
                }
                Err(error) => Err(unavailable(error)),
            },
            ScheduleEffect::Remove { schedule } => match self.owned(schedule) {
                Err(OrcaError::ScheduleNotFound) => applied(schedule, false),
                Ok(_) | Err(OrcaError::NotKitchenOwned) => Ok(Lookup::Unknown),
                Err(error) => Err(unavailable(error)),
            },
            ScheduleEffect::Trial { .. } => Ok(Lookup::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Text;

    #[test]
    fn names_round_trip_only_for_the_same_house() -> Result<(), Box<dyn std::error::Error>> {
        let home = HouseId::new("home")?;
        let other = HouseId::new("other")?;
        let name = ConsumerId::new("pickup")?;
        let native = native_schedule_name(&home, &name);
        assert_eq!(native, "kitchen:home:pickup");
        assert_eq!(decode_name(&home, &native), Some(name));
        assert_eq!(decode_name(&other, &native), None);
        assert_eq!(decode_name(&home, "Origin89 issue coordinator"), None);
        assert_eq!(decode_name(&home, "kitchen:home:"), None);
        assert_eq!(decode_name(&home, "kitchen:homely:pickup"), None);
        Ok(())
    }

    #[test]
    fn precheck_arguments_are_single_quoted() -> Result<(), Box<dyn std::error::Error>> {
        let argv = vec![
            Text::new("kitchen")?,
            Text::new("it's; rm -rf /")?,
            Text::new("$(x)")?,
        ];
        assert_eq!(
            shell_command(&argv),
            r"'kitchen' 'it'\''s; rm -rf /' '$(x)'"
        );
        Ok(())
    }

    #[test]
    fn launch_completion_is_not_readiness() -> Result<(), serde_json::Error> {
        let outcome =
            |run: Value| serde_json::from_value::<WireRun>(run).map(|run| run_outcome(&run));
        let precheck = |result: Value| {
            outcome(serde_json::json!({"status": "skipped_precheck", "precheckResult": result}))
        };
        assert_eq!(
            outcome(serde_json::json!({"status": "completed"}))?,
            RunOutcome::LaunchReported
        );
        assert_eq!(
            outcome(serde_json::json!({"status": "dispatch_failed"}))?,
            RunOutcome::LaunchFailed
        );
        assert_eq!(
            precheck(serde_json::json!({"exitCode": 1, "timedOut": false, "error": null}))?,
            RunOutcome::PrecheckIdle
        );
        for failed in [
            serde_json::json!({"exitCode": 2, "timedOut": false, "error": null}),
            serde_json::json!({"exitCode": null, "timedOut": true, "error": null}),
            serde_json::json!({"exitCode": 1, "timedOut": false, "error": "spawn ENOENT"}),
            serde_json::json!({"exitCode": null, "timedOut": false}),
        ] {
            assert_eq!(precheck(failed)?, RunOutcome::PrecheckFailed);
        }
        assert_eq!(
            outcome(serde_json::json!({"status": "skipped_precheck"}))?,
            RunOutcome::Unknown,
            "a skip without a recorded result is not idle"
        );
        assert_eq!(
            outcome(serde_json::json!({"status": "skipped_missed"}))?,
            RunOutcome::Skipped
        );
        assert_eq!(
            outcome(serde_json::json!({"status": "brand_new"}))?,
            RunOutcome::Unknown
        );
        Ok(())
    }
}
