//! Schedules as Orca automations.
//!
//! Kitchen names each automation `kitchen:<house>:<consumer>`, after the
//! workflow consumer scope it serves, and acts only on
//! automations whose name decodes for its own house, so existing automations
//! are never touched. Orca's automation commands take no request key, so:
//!
//! - an install holds a reservation for the house and consumer, so two
//!   installers cannot both list, find nothing, and both create;
//! - it then reconciles against a complete listing with [`plan_install`]. An
//!   existing schedule is reused only when it is paused and matches the
//!   requested definition; an active or different one is refused, never
//!   changed;
//! - a create whose response is lost is resolved by listing again, and stays
//!   [`OrcaError::InstallUncertain`] when the listing cannot show it;
//! - every change is read back before it is reported as done.
//!
//! Installs are always created disabled. Enabling is a separate call that
//! needs its own authority. Every recurrence is sent as one five-field cron
//! expression ([`crate::scheduling::Recurrence::cron`]). Orca 1.4.212 lists a
//! cron trigger unchanged in the automation's `rrule` (as its existing
//! automations show), so an installed schedule can be compared with a
//! requested one without knowing how Orca encodes presets. That a schedule
//! this adapter creates lists back identically has not been observed live,
//! because creating one needs authorization; the definition check fails
//! closed, naming the fields that differ, if it does not.

use std::num::NonZeroU32;

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
        Readiness, RunOutcome, ScheduleEvidence, ScheduleField, ScheduleObservation, ScheduleRun,
        ScheduleSpec, ScheduleState, ScheduleUsage, ScheduleWorkspace, plan_install,
    },
    trust::Measurement,
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

/// One automation as `automations list` reports it. Fields Kitchen compares
/// against a requested definition are optional: a listing that omits one
/// cannot confirm it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Automation {
    id: String,
    name: String,
    enabled: bool,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    /// The trigger: a cron expression stays as given.
    #[serde(default)]
    rrule: Option<String>,
    #[serde(default)]
    missed_run_grace_minutes: Option<u64>,
    #[serde(default)]
    reuse_session: Option<bool>,
    #[serde(default)]
    precheck: Option<StoredPrecheck>,
    #[serde(default)]
    workspace_mode: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPrecheck {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
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
    #[serde(default)]
    id: Option<String>,
    status: String,
    #[serde(default)]
    scheduled_for: Option<u64>,
    #[serde(default)]
    created_at: Option<u64>,
    #[serde(default)]
    precheck_result: Option<WirePrecheck>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

/// A run's usage as Orca 1.4.212 reports it: `status` is `known` with token
/// counts, or `unavailable` with a reason such as `no_matching_session`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireUsage {
    status: String,
    #[serde(default)]
    total_tokens: Option<u64>,
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

/// A run's reported tokens. Only a `known` report with a total is an
/// observation; an unavailable report stays unavailable and an absent one
/// missing, never zero.
fn run_usage(run: &WireRun) -> Measurement<u64> {
    let Some(usage) = &run.usage else {
        return Measurement::Missing;
    };
    let source = run
        .id
        .as_deref()
        .and_then(|id| ExternalRef::new(&format!("orca-run:{id}")).ok());
    match (usage.status.as_str(), usage.total_tokens, source) {
        ("known", Some(value), Some(source)) => Measurement::Observed {
            value,
            samples: NonZeroU32::MIN,
            source,
        },
        _ => Measurement::Unavailable,
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

    /// This house's schedules among `automations`.
    fn installed_from(
        &self,
        automations: &[Automation],
    ) -> Result<Vec<InstalledSchedule>, OrcaError> {
        let house = &self.config().house;
        automations
            .iter()
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

    /// Every Kitchen schedule installed for this house.
    ///
    /// # Errors
    /// Call and parse failures, and [`OrcaError::ListingTooLong`].
    pub fn installed_schedules(&self) -> Result<Vec<InstalledSchedule>, OrcaError> {
        self.installed_from(&self.automations()?)
    }

    /// The schedule an install of `spec` may reuse, or `None` when it may
    /// create one.
    ///
    /// Reuse needs the installed schedule to be paused, because a disabled
    /// install must never leave a consumer firing, and to match the requested
    /// definition, because reusing a different one would install nothing
    /// that was asked for.
    fn existing_install(
        &self,
        spec: &ScheduleSpec,
        automations: &[Automation],
    ) -> Result<Option<ResourceRef>, OrcaError> {
        match plan_install(spec.consumer(), &self.installed_from(automations)?) {
            InstallPlan::Create => Ok(None),
            InstallPlan::Duplicates(duplicates) => Err(OrcaError::DuplicateSchedules {
                count: duplicates.len(),
            }),
            InstallPlan::Installed(existing) => {
                let automation = automations
                    .iter()
                    .find(|automation| automation.id == existing.handle.as_str())
                    .ok_or(OrcaError::ScheduleNotFound)?;
                if automation.enabled {
                    return Err(OrcaError::ScheduleActive);
                }
                let fields = self.definition_gaps(spec, automation);
                if fields.is_empty() {
                    Ok(Some(existing))
                } else {
                    Err(OrcaError::ScheduleDiffers { fields })
                }
            }
        }
    }

    /// The parts of `spec` that `automation` does not show. A part the
    /// listing omits counts as a gap: it cannot be confirmed.
    fn definition_gaps(&self, spec: &ScheduleSpec, automation: &Automation) -> Vec<ScheduleField> {
        let config = self.config();
        let precheck = match (spec.precheck(), &automation.precheck) {
            (None, None) => true,
            (Some(wanted), Some(stored)) => {
                stored.command == shell_command(wanted.argv())
                    && stored.timeout_seconds == Some(wanted.timeout().whole_seconds())
            }
            (None, Some(_)) | (Some(_), None) => false,
        };
        let workspace = match spec.workspace() {
            ScheduleWorkspace::NewPerRun => {
                automation.workspace_mode.as_deref() == Some("new-per-run")
                    && automation.base_branch.as_deref()
                        == config.base_branch.as_ref().map(ExternalRef::as_str)
                    // Only an `id:` selector can be compared with the stored project.
                    && config
                        .repo
                        .as_str()
                        .strip_prefix("id:")
                        .is_none_or(|id| automation.project_id.as_deref() == Some(id))
            }
            ScheduleWorkspace::Existing(existing) => {
                automation.workspace_mode.as_deref() == Some("existing")
                    && automation.workspace_id.as_deref() == Some(existing.handle.as_str())
            }
        };
        [
            (
                automation.prompt.as_deref() == Some(spec.prompt().as_str()),
                ScheduleField::Prompt,
            ),
            (
                automation.agent_id.as_deref() == Some(spec.agent().selection.agent.as_str()),
                ScheduleField::Agent,
            ),
            (
                automation.rrule.as_deref() == Some(spec.recurrence().cron().as_str()),
                ScheduleField::Recurrence,
            ),
            (
                automation.timezone.as_deref() == Some(spec.timezone().as_str()),
                ScheduleField::Timezone,
            ),
            (precheck, ScheduleField::Precheck),
            (workspace, ScheduleField::Workspace),
            (
                automation.missed_run_grace_minutes
                    == Some(u64::from(spec.missed_run_grace().get())),
                ScheduleField::MissedRunGrace,
            ),
            (
                automation.reuse_session == Some(spec.reuse_session()),
                ScheduleField::SessionReuse,
            ),
        ]
        .into_iter()
        .filter_map(|(matches, field)| (!matches).then_some(field))
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
            .value("provider", spec.agent().selection.agent.as_str())
            .value("timezone", spec.timezone().as_str())
            .value(
                "missed-run-grace-minutes",
                &spec.missed_run_grace().get().to_string(),
            )
            .value("trigger", &spec.recurrence().cron())
            .switch("disabled");
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

    /// Install `spec` paused, or return the paused schedule already installed
    /// for its consumer with the same definition.
    ///
    /// A selection naming a model or effort is refused before anything is
    /// read or reserved: Orca automations take only a provider
    /// ([`SCHEDULE_SELECTION`](super::SCHEDULE_SELECTION)).
    ///
    /// Concurrent installers for one house and consumer take turns: the
    /// listing, the create, and the read-back happen under a reservation, so
    /// a second installer finds the schedule the first created.
    ///
    /// # Errors
    /// [`OrcaError::Selection`] naming every part of the selection Orca
    /// cannot launch. [`OrcaError::ScheduleActive`] when the consumer's
    /// schedule is firing and [`OrcaError::ScheduleDiffers`] when it is not
    /// the requested one;
    /// neither changes anything. [`OrcaError::DuplicateSchedules`] when
    /// several share the name, [`OrcaError::InstallUncertain`] when a create
    /// may have happened but no listing shows it,
    /// [`OrcaError::StateMismatch`] when a new schedule does not read back
    /// paused, [`OrcaError::ReservationBusy`] when another installer held the
    /// reservation for the whole wait, and call or parse failures.
    pub fn install_schedule(&self, spec: &ScheduleSpec) -> Result<ResourceRef, OrcaError> {
        self.install(spec).map(|(schedule, _)| schedule)
    }

    /// [`OrcaBackend::install_schedule`], and whether this call created the
    /// schedule or reused one already installed.
    fn install(&self, spec: &ScheduleSpec) -> Result<(ResourceRef, Install), OrcaError> {
        backend::SCHEDULE_SELECTION.check(&spec.agent().selection)?;
        let consumer = spec.consumer();
        let mut reservation = self.reserve(format!(
            "schedule-{:032x}",
            backend::key_digest(&self.config().house, consumer.as_str())
        ))?;
        let listed = self.automations()?;
        if let Some(policy) = self.schedule_policy() {
            policy.check_install(spec, &self.installed_from(&listed)?)?;
        }
        if let Some(existing) = self.existing_install(spec, &listed)? {
            reservation.settle();
            return Ok((existing, Install::Reused));
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
        // Under the reservation the listing before the create showed none,
        // so the one listed now is this call's.
        if paused {
            reservation.settle();
            Ok((resource, Install::Created))
        } else {
            Err(OrcaError::StateMismatch)
        }
    }

    /// Observe a schedule's state and up to [`MAX_SCHEDULE_RUNS`] recent
    /// runs, each judged against `readiness`.
    ///
    /// Orca's `completed` only means the launch step finished. A run whose
    /// agent showed no [`crate::scheduling::ReadinessSignal`] within the
    /// deadline is [`crate::scheduling::RunVerdict::LaunchFailed`], so a
    /// launch swallowed by an interactive prompt is reported as one instead
    /// of as a completed run.
    ///
    /// # Errors
    /// Call and parse failures. A schedule absent from a complete listing is
    /// reported as [`ObservedScheduleState::Missing`], not an error.
    pub fn inspect_schedule(
        &self,
        schedule: &ResourceRef,
        readiness: &Readiness<'_>,
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
        // Newest first. A run with no due time is a trial or still
        // dispatching, so it ranks newest and survives the cut below; the
        // sort is stable, so undated runs keep Orca's listing order.
        runs.runs
            .sort_by_key(|run| std::cmp::Reverse((run.scheduled_for.is_none(), run.scheduled_for)));
        let recent: Vec<ScheduleRun> = runs
            .runs
            .iter()
            .take(MAX_SCHEDULE_RUNS)
            .map(|run| ScheduleRun {
                outcome: run_outcome(run),
                scheduled_for: run.scheduled_for.map(Timestamp::from_unix_millis),
                created_at: run.created_at.map(Timestamp::from_unix_millis),
                usage: run_usage(run),
                // Orca's run records carry no provider, and the automation's
                // current one may have been edited since the run.
                agent: None,
            })
            .collect();
        Ok(ScheduleObservation {
            state: state_of(automation.enabled),
            recent_runs: readiness.judge(&recent),
        })
    }

    /// Pause or activate a Kitchen schedule and read the state back.
    ///
    /// Activation starts a live consumer; the caller must hold that authority.
    /// With a schedule policy, activation is refused while the schedule's or
    /// the house's budget is exhausted in the current window, or cannot be
    /// shown to hold, before anything is edited. Pausing is never refused.
    ///
    /// # Errors
    /// [`OrcaError::NotKitchenOwned`], [`OrcaError::ScheduleNotFound`],
    /// [`OrcaError::ScheduleLimit`] for a refused activation,
    /// [`OrcaError::StateMismatch`] when the read-back differs, and call failures.
    pub fn set_schedule_state(
        &self,
        schedule: &ResourceRef,
        state: ScheduleState,
    ) -> Result<(), OrcaError> {
        let automation = self.owned(schedule)?;
        if state == ScheduleState::Active {
            self.check_activation(&automation)?;
        }
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

    /// Observe every Kitchen schedule of this house and its recent runs now,
    /// for judging budgets: an activation check or a budget pass
    /// ([`crate::workflows::budget`]). Reads only.
    ///
    /// Runs are judged with no readiness signals, so budgets count every
    /// launched run as a possible agent start. Every schedule is judged the
    /// same way, the budget schedule included.
    ///
    /// # Errors
    /// Call and parse failures, and [`OrcaError::ListingTooLong`].
    pub fn schedule_evidence(&self) -> Result<ScheduleEvidence, OrcaError> {
        let now = self.now();
        let readiness = Readiness::new(&[], now, std::time::Duration::ZERO);
        let schedules = self
            .installed_schedules()?
            .into_iter()
            .map(|installed| {
                Ok(ScheduleUsage {
                    observation: self.inspect_schedule(&installed.resource, &readiness)?,
                    consumer: installed.consumer,
                    schedule: installed.resource,
                })
            })
            .collect::<Result<Vec<_>, OrcaError>>()?;
        Ok(ScheduleEvidence {
            house: self.config().house.clone(),
            observed_at: now,
            schedules,
        })
    }

    /// Judge the house's usage in the window containing now, from a fresh
    /// observation of every schedule, and refuse activating `automation`
    /// when its budget is exhausted or unverifiable.
    fn check_activation(&self, automation: &Automation) -> Result<(), OrcaError> {
        let Some(policy) = self.schedule_policy() else {
            return Ok(());
        };
        let house = &self.config().house;
        let Some(consumer) = decode_name(house, &automation.name) else {
            return Err(OrcaError::NotKitchenOwned);
        };
        let evidence = self.schedule_evidence()?;
        policy.check_activation(house, &evidence, &consumer)?;
        Ok(())
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
        | OrcaError::ScheduleActive
        | OrcaError::ScheduleDiffers { .. }
        | OrcaError::ReservationRedirected
        | OrcaError::ReservationInsideRepository
        | OrcaError::ReservationUnavailable(_)
        | OrcaError::BranchUnobtainable { .. }
        | OrcaError::BranchTaken { .. }
        | OrcaError::Schedule(_)
        | OrcaError::ScheduleLimit(_)
        | OrcaError::Selection(_)
        | OrcaError::Contract(_) => EffectFailure::NotApplied(NotAppliedReason::Rejected),
        // Nothing was sent, but the holder may be about to install it.
        OrcaError::Timeout | OrcaError::ReservationBusy => {
            EffectFailure::Uncertain(UncertainReason::Timeout)
        }
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
        | OrcaError::StateMismatch
        | OrcaError::WrongBranchRunning { .. } => {
            EffectFailure::Uncertain(UncertainReason::ResponseLost)
        }
    }
}

/// Whether an install created its schedule or reused one already installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Install {
    Created,
    Reused,
}

/// A receipt referenced by the automation id. An install that created the
/// schedule lists it as created; a reused schedule and every other change
/// touch an existing one, which does not make the task its owner.
fn schedule_receipt(
    schedule: &ResourceRef,
    install: Option<Install>,
) -> Result<Receipt, EffectFailure> {
    let (created, touched) = match install {
        Some(Install::Created) => (vec![schedule.clone()], Vec::new()),
        Some(Install::Reused) | None => (Vec::new(), vec![schedule.clone()]),
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
                let (installed, install) = self
                    .install(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(&installed, Some(install))
            }
            ScheduleEffect::SetState { schedule, state } => {
                self.set_schedule_state(schedule, *state)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, None)
            }
            ScheduleEffect::Remove { schedule } => {
                self.remove_schedule(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, None)
            }
            ScheduleEffect::Trial { schedule } => {
                self.trial_schedule(schedule)
                    .map_err(|error| schedule_failure(&error))?;
                schedule_receipt(schedule, None)
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
        let applied = |schedule: &ResourceRef, install: Option<Install>| {
            schedule_receipt(schedule, install)
                .map(Lookup::Applied)
                .map_err(|_| crate::contracts::BackendUnavailable::Transport)
        };
        match effect {
            ScheduleEffect::InstallDisabled { schedule } => {
                // The lookup matches on the provider alone, so it must refuse
                // a selection an install would have refused: Orca cannot
                // have launched a model or effort it does not take.
                if backend::SCHEDULE_SELECTION
                    .check(&schedule.agent().selection)
                    .is_err()
                {
                    return Ok(Lookup::Unknown);
                }
                let listed = self.automations().map_err(unavailable)?;
                // Applied only when an install would reuse the schedule: one
                // that is active or different is not what was requested. The
                // listing cannot show whether this key created it, so it is
                // reported reused: creation is never claimed without proof.
                match self.existing_install(schedule, &listed) {
                    Ok(Some(existing)) => applied(&existing, Some(Install::Reused)),
                    Ok(None)
                    | Err(
                        OrcaError::ScheduleActive
                        | OrcaError::ScheduleDiffers { .. }
                        | OrcaError::DuplicateSchedules { .. },
                    ) => Ok(Lookup::Unknown),
                    Err(error) => Err(unavailable(error)),
                }
            }
            ScheduleEffect::SetState { schedule, state } => match self.owned(schedule) {
                Ok(automation) if automation.enabled == matches!(state, ScheduleState::Active) => {
                    applied(schedule, None)
                }
                Ok(_) | Err(OrcaError::ScheduleNotFound | OrcaError::NotKitchenOwned) => {
                    Ok(Lookup::Unknown)
                }
                Err(error) => Err(unavailable(error)),
            },
            ScheduleEffect::Remove { schedule } => match self.owned(schedule) {
                Err(OrcaError::ScheduleNotFound) => applied(schedule, None),
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
