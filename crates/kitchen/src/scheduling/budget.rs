//! House limits on scheduled work: a minimum interval between runs and
//! usage budgets per schedule and per house over a fixed window.
//!
//! Every scheduled run that passes its precheck starts an agent, so a short
//! interval or a precheck that rarely finds work can spend a house's usage
//! without producing anything. A [`SchedulePolicy`] bounds that spend:
//!
//! - Installing a schedule that fires more often than its minimum interval,
//!   or whose budget would overcommit the house budget, is refused naming the
//!   limit ([`SchedulePolicy::check_install`]). Per-schedule budgets are
//!   allocations of the house budget, so their sum cannot exceed it.
//! - An exhausted budget pauses the schedule through a
//!   [`ScheduleEffect::SetState`] effect and is reported to the owner once per
//!   schedule and window ([`SchedulePolicy::plan_exhaustion`]). Resuming is
//!   the owner's separate activation, which
//!   [`SchedulePolicy::check_activation`] refuses while the budget stays
//!   exhausted.
//!
//! The house's budget schedule is an ordinary schedule: its runs count toward
//! its own budget and the house budget, and it holds an allocation of the
//! house budget. When the house budget is exhausted the budget pass reports
//! first and then pauses every exhausted schedule, its own tick included. The
//! owner reactivates the tick after the window resets; no pass ever
//! reactivates a schedule.
//!
//! Budgets count agent runs and, where the backend reports them, tokens. Run
//! counts come from the retained run history ([`MAX_SCHEDULE_RUNS`] runs per
//! schedule), so a window that history does not reach back through holds lower
//! bounds only: activation is refused, and doctor reports the run budget as
//! unverifiable, unless the budget is already shown exhausted. Token usage
//! the backend does not report is unknown, never zero: it can neither prove a
//! budget exhausted nor prove it within bounds, and doctor reports a token
//! budget it cannot enforce.
//!
//! Windows are fixed and aligned to the Unix epoch in UTC, so every caller
//! computes the same window for the same instant. Intervals are measured in
//! the schedule's wall-clock time; a daylight-saving change can shorten one
//! interval by the size of the shift.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::{NonZeroU32, NonZeroU64},
};

use serde::{Deserialize, Serialize};

use crate::{
    ConsumerId, ErrorClass, HouseId, WorkflowId,
    contracts::{Effect, ExternalRef, ResourceRef, ScheduleEffect, Timestamp},
    scheduling::{
        InstalledSchedule, JudgedRun, MAX_SCHEDULE_RUNS, ObservedScheduleState, Recurrence,
        RunVerdict, ScheduleObservation, ScheduleRun, ScheduleSpec, ScheduleState,
    },
    state::{MarkerFact, MarkerKey, MarkerSchema, MarkerSubject, WorkItem},
    trust::Measurement,
};

/// Longest accepted usage window: 31 days.
pub const MAX_WINDOW_HOURS: u16 = 31 * 24;

/// Longest accepted minimum interval: 31 days.
pub const MAX_INTERVAL_MINUTES: u32 = 31 * 24 * 60;

/// Most per-schedule entries one policy may hold.
pub const MAX_SCHEDULE_LIMITS: usize = 256;

/// Most schedules one [`ScheduleEvidence`] may describe.
pub const MAX_EVIDENCE_SCHEDULES: usize = 500;

/// The workflow that records budget exhaustion reports.
pub const BUDGET_WORKFLOW: &str = "schedule-budget";

const MINUTES_PER_DAY: u32 = 24 * 60;

/// One limit a [`SchedulePolicy`] sets. Refusals and reports name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleLimit {
    /// The minimum interval set for one schedule.
    ScheduleMinInterval,
    /// The minimum interval every schedule of the house must keep.
    HouseMinInterval,
    /// Agent runs one schedule may start per window.
    ScheduleRuns,
    /// Agent runs all of the house's schedules may start per window.
    HouseRuns,
    /// Tokens one schedule may use per window.
    ScheduleTokens,
    /// Tokens all of the house's schedules may use per window.
    HouseTokens,
}

impl fmt::Display for ScheduleLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ScheduleMinInterval => "schedule minimum interval",
            Self::HouseMinInterval => "house minimum interval",
            Self::ScheduleRuns => "schedule run budget",
            Self::HouseRuns => "house run budget",
            Self::ScheduleTokens => "schedule token budget",
            Self::HouseTokens => "house token budget",
        })
    }
}

/// A refused schedule policy check. Messages name the limit and the numbers
/// involved; they never echo prompts or other private input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BudgetError {
    /// The policy is malformed or out of bounds.
    #[error("invalid schedule policy")]
    InvalidPolicy,
    /// A per-schedule entry would relax a house limit.
    #[error("a schedule entry relaxes the {limit}")]
    Relaxation {
        /// The house limit the entry would relax.
        limit: ScheduleLimit,
    },
    /// The recurrence could not be read to measure its interval.
    #[error("the schedule's recurrence cannot be read to check its interval")]
    UnreadableRecurrence,
    /// The schedule fires more often than its minimum interval allows.
    #[error(
        "schedule {consumer} breaks the {limit}: it can fire {actual_minutes} minutes apart, the minimum is {required_minutes}"
    )]
    IntervalTooShort {
        /// The schedule.
        consumer: ConsumerId,
        /// The violated limit.
        limit: ScheduleLimit,
        /// The shortest gap between two of its runs.
        actual_minutes: u32,
        /// The minimum the policy requires.
        required_minutes: u32,
    },
    /// Installing the schedule would allocate more than the house budget.
    #[error("schedule {consumer} would overcommit the {limit}: {allocated} allocated of {allowed}")]
    Overcommitted {
        /// The schedule.
        consumer: ConsumerId,
        /// The violated limit.
        limit: ScheduleLimit,
        /// The sum of allocations including this schedule.
        allocated: u64,
        /// The house budget.
        allowed: u64,
    },
    /// The schedule may not run again in this window.
    #[error("schedule {consumer} has exhausted the {limit} for this window: {used} of {allowed}")]
    Exhausted {
        /// The schedule.
        consumer: ConsumerId,
        /// The exhausted limit.
        limit: ScheduleLimit,
        /// Usage observed in the window.
        used: u64,
        /// The budget.
        allowed: u64,
    },
    /// The observed runs may not be all of the window's runs, so the budget
    /// cannot be shown to hold.
    #[error(
        "schedule {consumer}'s run history does not reach the start of the window ({observed} runs seen of {allowed} allowed), so it may already be over budget"
    )]
    IncompleteEvidence {
        /// The schedule.
        consumer: ConsumerId,
        /// Runs observed in the window: a lower bound.
        observed: u32,
        /// The run budget that cannot be verified.
        allowed: u64,
    },
    /// No usage evidence was supplied for the schedule being checked.
    #[error("schedule {consumer} has no usage evidence")]
    UnobservedSchedule {
        /// The schedule.
        consumer: ConsumerId,
    },
    /// The usage evidence belongs to another house.
    #[error("schedule usage evidence belongs to another house")]
    HouseMismatch,
    /// The usage evidence is oversized or names a schedule twice.
    #[error("schedule usage evidence is oversized or names a schedule twice")]
    InvalidEvidence,
}

impl BudgetError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidPolicy | Self::UnreadableRecurrence | Self::InvalidEvidence => {
                ErrorClass::InvalidInput
            }
            Self::Relaxation { .. }
            | Self::IntervalTooShort { .. }
            | Self::Overcommitted { .. }
            | Self::Exhausted { .. }
            | Self::IncompleteEvidence { .. }
            | Self::UnobservedSchedule { .. }
            | Self::HouseMismatch => ErrorClass::Refused,
        }
    }
}

/// The length of a usage window in hours, 1 to [`MAX_WINDOW_HOURS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct WindowHours(u16);

impl WindowHours {
    /// Validate a window length.
    ///
    /// # Errors
    /// Returns [`BudgetError::InvalidPolicy`] outside 1 to [`MAX_WINDOW_HOURS`].
    pub const fn new(hours: u16) -> Result<Self, BudgetError> {
        if hours == 0 || hours > MAX_WINDOW_HOURS {
            return Err(BudgetError::InvalidPolicy);
        }
        Ok(Self(hours))
    }

    /// The length in hours.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The window containing `now`: fixed windows aligned to the Unix epoch.
    #[must_use]
    pub fn containing(self, now: Timestamp) -> UsageWindow {
        // At most 744 hours, so this cannot overflow.
        let length = u64::from(self.0) * 3_600_000;
        let millis = now.as_unix_millis();
        let start = millis - millis % length;
        UsageWindow {
            start: Timestamp::from_unix_millis(start),
            end: Timestamp::from_unix_millis(start.saturating_add(length)),
        }
    }
}

impl TryFrom<u16> for WindowHours {
    type Error = BudgetError;

    fn try_from(hours: u16) -> Result<Self, Self::Error> {
        Self::new(hours)
    }
}

impl From<WindowHours> for u16 {
    fn from(hours: WindowHours) -> Self {
        hours.0
    }
}

/// One fixed usage window, `start` inclusive and `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageWindow {
    /// First instant in the window.
    pub start: Timestamp,
    /// First instant after it.
    pub end: Timestamp,
}

impl UsageWindow {
    /// Whether `at` falls in this window.
    #[must_use]
    pub fn contains(&self, at: Timestamp) -> bool {
        self.start <= at && at < self.end
    }
}

/// A minimum interval between runs in minutes, 1 to [`MAX_INTERVAL_MINUTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct IntervalMinutes(u32);

impl IntervalMinutes {
    /// Validate an interval.
    ///
    /// # Errors
    /// Returns [`BudgetError::InvalidPolicy`] outside 1 to [`MAX_INTERVAL_MINUTES`].
    pub const fn new(minutes: u32) -> Result<Self, BudgetError> {
        if minutes == 0 || minutes > MAX_INTERVAL_MINUTES {
            return Err(BudgetError::InvalidPolicy);
        }
        Ok(Self(minutes))
    }

    /// The interval in minutes.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for IntervalMinutes {
    type Error = BudgetError;

    fn try_from(minutes: u32) -> Result<Self, Self::Error> {
        Self::new(minutes)
    }
}

impl From<IntervalMinutes> for u32 {
    fn from(minutes: IntervalMinutes) -> Self {
        minutes.0
    }
}

/// Usage allowed per window. Runs are agent runs: a run whose precheck
/// reported idle or failed, or that was skipped before launch, started no
/// agent and does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    /// Agent runs allowed per window.
    pub runs: NonZeroU32,
    /// Tokens allowed per window; `None` sets no token limit. A token limit
    /// is enforced only as far as the backend reports usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<NonZeroU64>,
}

impl Budget {
    /// Whether `other` fits within this budget in every dimension. A missing
    /// token limit in `other` does not fit under a token limit here.
    const fn covers(&self, other: &Self) -> bool {
        let tokens = match (self.tokens, other.tokens) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(limit), Some(wanted)) => wanted.get() <= limit.get(),
        };
        other.runs.get() <= self.runs.get() && tokens
    }
}

/// Limits set for one schedule, tighter than the house defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleLimits {
    /// Minimum interval for this schedule; at least the house minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_interval_minutes: Option<IntervalMinutes>,
    /// This schedule's allocation of the house budget, instead of the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

/// A share of runs in percent, 1 to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Percent(u8);

impl Percent {
    /// Validate a percentage.
    ///
    /// # Errors
    /// Returns [`BudgetError::InvalidPolicy`] outside 1 to 100.
    pub const fn new(value: u8) -> Result<Self, BudgetError> {
        if value == 0 || value > 100 {
            return Err(BudgetError::InvalidPolicy);
        }
        Ok(Self(value))
    }

    /// The percentage.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Percent {
    type Error = BudgetError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Percent> for u8 {
    fn from(value: Percent) -> Self {
        value.0
    }
}

/// When doctor recommends revisiting a mostly idle schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdlePolicy {
    /// Share of recent runs whose precheck reported idle.
    pub share_percent: Percent,
    /// Fewest recent runs before a share is judged.
    pub min_runs: NonZeroU32,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            share_percent: Percent(80),
            min_runs: NonZeroU32::MIN.saturating_add(9),
        }
    }
}

/// A house's limits on scheduled work. Stored in house configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchedulePolicy {
    /// Length of the fixed usage window budgets apply to.
    pub window_hours: WindowHours,
    /// Minimum interval every schedule of the house must keep.
    pub min_interval_minutes: IntervalMinutes,
    /// Usage all of the house's schedules together may spend per window.
    pub house_budget: Budget,
    /// Each schedule's default allocation of the house budget.
    pub schedule_budget: Budget,
    /// Tighter limits for individual schedules, by consumer scope.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schedules: BTreeMap<ConsumerId, ScheduleLimits>,
    /// When doctor recommends revisiting a mostly idle schedule.
    #[serde(default)]
    pub idle: IdlePolicy,
}

impl SchedulePolicy {
    /// Check bounds and that no schedule entry relaxes a house limit.
    ///
    /// # Errors
    /// [`BudgetError::InvalidPolicy`] for too many entries, and
    /// [`BudgetError::Relaxation`] for an interval below the house minimum or
    /// an allocation larger than the house budget.
    pub fn validate(&self) -> Result<(), BudgetError> {
        if self.schedules.len() > MAX_SCHEDULE_LIMITS {
            return Err(BudgetError::InvalidPolicy);
        }
        let budgets = std::iter::once(&self.schedule_budget).chain(
            self.schedules
                .values()
                .filter_map(|limits| limits.budget.as_ref()),
        );
        for budget in budgets {
            if budget.runs > self.house_budget.runs {
                return Err(BudgetError::Relaxation {
                    limit: ScheduleLimit::HouseRuns,
                });
            }
            if !self.house_budget.covers(budget) {
                return Err(BudgetError::Relaxation {
                    limit: ScheduleLimit::HouseTokens,
                });
            }
        }
        let relaxed_interval = self.schedules.values().any(|limits| {
            limits
                .min_interval_minutes
                .is_some_and(|minutes| minutes < self.min_interval_minutes)
        });
        if relaxed_interval {
            return Err(BudgetError::Relaxation {
                limit: ScheduleLimit::HouseMinInterval,
            });
        }
        Ok(())
    }

    /// The budget `consumer` is allocated.
    #[must_use]
    pub fn budget_for(&self, consumer: &ConsumerId) -> Budget {
        self.schedules
            .get(consumer)
            .and_then(|limits| limits.budget)
            .unwrap_or(self.schedule_budget)
    }

    /// Refuse installing or updating `spec` when it breaks a limit.
    ///
    /// `installed` must be the complete inventory of the house's schedules,
    /// the budget schedule included; an entry for the same consumer is the
    /// schedule being updated and is not counted twice. Schedules the backend
    /// reports missing hold no allocation.
    ///
    /// # Errors
    /// [`BudgetError::IntervalTooShort`] naming the schedule or house
    /// minimum interval, [`BudgetError::Overcommitted`] naming the house run
    /// or token budget, [`BudgetError::UnreadableRecurrence`], and any
    /// [`Self::validate`] refusal.
    pub fn check_install(
        &self,
        spec: &ScheduleSpec,
        installed: &[InstalledSchedule],
    ) -> Result<(), BudgetError> {
        self.validate()?;
        let consumer = spec.consumer();
        self.check_interval(spec)?;
        let others: BTreeSet<&ConsumerId> = installed
            .iter()
            .filter(|schedule| {
                schedule.state != ObservedScheduleState::Missing && &schedule.consumer != consumer
            })
            .map(|schedule| &schedule.consumer)
            .collect();
        let allocations: Vec<Budget> = others
            .into_iter()
            .chain(std::iter::once(consumer))
            .map(|scheduled| self.budget_for(scheduled))
            .collect();
        let runs = allocations.iter().fold(0_u64, |sum, budget| {
            sum.saturating_add(budget.runs.get().into())
        });
        let allowed_runs = u64::from(self.house_budget.runs.get());
        if runs > allowed_runs {
            return Err(BudgetError::Overcommitted {
                consumer: consumer.clone(),
                limit: ScheduleLimit::HouseRuns,
                allocated: runs,
                allowed: allowed_runs,
            });
        }
        if let Some(allowed) = self.house_budget.tokens {
            // Validation guarantees every allocation has a token limit here.
            let tokens = allocations.iter().fold(0_u64, |sum, budget| {
                sum.saturating_add(budget.tokens.map_or(u64::MAX, NonZeroU64::get))
            });
            if tokens > allowed.get() {
                return Err(BudgetError::Overcommitted {
                    consumer: consumer.clone(),
                    limit: ScheduleLimit::HouseTokens,
                    allocated: tokens,
                    allowed: allowed.get(),
                });
            }
        }
        Ok(())
    }

    fn check_interval(&self, spec: &ScheduleSpec) -> Result<(), BudgetError> {
        let consumer = spec.consumer();
        let (limit, required) = match self
            .schedules
            .get(consumer)
            .and_then(|limits| limits.min_interval_minutes)
        {
            Some(minutes) => (ScheduleLimit::ScheduleMinInterval, minutes),
            None => (ScheduleLimit::HouseMinInterval, self.min_interval_minutes),
        };
        if let Some(actual) = shortest_interval_minutes(spec.recurrence())?
            && actual < required.get()
        {
            return Err(BudgetError::IntervalTooShort {
                consumer: consumer.clone(),
                limit,
                actual_minutes: actual,
                required_minutes: required.get(),
            });
        }
        Ok(())
    }

    /// Judge each schedule's usage in the window containing the evidence's
    /// observation time, against its budget and the house budget.
    ///
    /// # Errors
    /// [`BudgetError::HouseMismatch`] for another house's evidence, any
    /// [`ScheduleEvidence::validate`] refusal, and any [`Self::validate`]
    /// refusal.
    pub fn assess(
        &self,
        house: &HouseId,
        evidence: &ScheduleEvidence,
    ) -> Result<BudgetAssessment, BudgetError> {
        self.validate()?;
        if &evidence.house != house {
            return Err(BudgetError::HouseMismatch);
        }
        evidence.validate()?;
        let window = self.window_hours.containing(evidence.observed_at);
        let mut house_usage = WindowUsage {
            runs: 0,
            tokens: TokenUsage::default(),
            complete: true,
        };
        let mut schedules: Vec<ScheduleAssessment> = evidence
            .schedules
            .iter()
            .map(|schedule| {
                let usage = WindowUsage::of(&schedule.observation, window);
                house_usage = house_usage.plus(&usage);
                let exhausted = exhaustion(
                    &self.budget_for(&schedule.consumer),
                    &usage,
                    ScheduleLimit::ScheduleRuns,
                    ScheduleLimit::ScheduleTokens,
                );
                ScheduleAssessment {
                    consumer: schedule.consumer.clone(),
                    schedule: schedule.schedule.clone(),
                    state: schedule.observation.state,
                    usage,
                    exhausted,
                }
            })
            .collect();
        let house_exhausted = exhaustion(
            &self.house_budget,
            &house_usage,
            ScheduleLimit::HouseRuns,
            ScheduleLimit::HouseTokens,
        );
        for schedule in &mut schedules {
            if schedule.exhausted.is_none() {
                schedule.exhausted = house_exhausted;
            }
        }
        Ok(BudgetAssessment {
            window,
            house: house_usage,
            house_exhausted,
            schedules,
        })
    }

    /// Refuse activating `consumer` while its budget or the house budget is
    /// exhausted in the current window. Evidence must cover every schedule
    /// of the house, since the house budget counts them all.
    ///
    /// A window whose start the observation did not reach
    /// ([`WindowUsage::complete`] is `false`) holds lower bounds only, so
    /// activation is refused unless the budget is already shown exhausted.
    ///
    /// # Errors
    /// [`BudgetError::Exhausted`] naming the limit,
    /// [`BudgetError::IncompleteEvidence`] for an incomplete window,
    /// [`BudgetError::UnobservedSchedule`] when `consumer` has no evidence,
    /// and any [`Self::assess`] refusal.
    pub fn check_activation(
        &self,
        house: &HouseId,
        evidence: &ScheduleEvidence,
        consumer: &ConsumerId,
    ) -> Result<(), BudgetError> {
        let assessment = self.assess(house, evidence)?;
        let schedule = assessment
            .schedules
            .iter()
            .find(|schedule| &schedule.consumer == consumer)
            .ok_or_else(|| BudgetError::UnobservedSchedule {
                consumer: consumer.clone(),
            })?;
        if let Some(exhausted) = schedule.exhausted {
            return Err(BudgetError::Exhausted {
                consumer: consumer.clone(),
                limit: exhausted.limit,
                used: exhausted.used,
                allowed: exhausted.allowed,
            });
        }
        // Not proven exhausted, but counts from a window whose start the
        // observation did not reach are lower bounds: fail closed.
        let (observed, allowed) = if !schedule.usage.complete {
            (schedule.usage.runs, self.budget_for(consumer).runs)
        } else if !assessment.house.complete {
            (assessment.house.runs, self.house_budget.runs)
        } else {
            return Ok(());
        };
        Err(BudgetError::IncompleteEvidence {
            consumer: consumer.clone(),
            observed,
            allowed: allowed.get().into(),
        })
    }

    /// The schedules to pause and report for exhausted budgets.
    ///
    /// Every pass pauses an exhausted schedule that is still active or whose
    /// state is unknown, including one re-activated after its report was
    /// recorded; a paused one is only reported. A missing one is neither.
    /// The owner report is sent once per window: `reported` says whether a
    /// marker is recorded under a key, and [`BudgetExhaustion::report_due`] is
    /// `false` when one is recorded under the report's
    /// [`BudgetExhaustion::marker_key`] or, for a report that had nowhere to
    /// go, its [`BudgetExhaustion::undeliverable_key`]. Either way the window
    /// asks nothing more of the owner's channel.
    ///
    /// A run budget above the retained run history cannot be proven exhausted
    /// from an incomplete window; see [`WindowUsage::complete`].
    ///
    /// The caller submits each pause effect, reports to the owner, and then
    /// records the marker. After an interruption, the next pass finds the
    /// schedule paused and unreported and reports it without pausing again.
    ///
    /// # Errors
    /// Any [`Self::assess`] refusal, and a marker value that cannot be built.
    pub fn plan_exhaustion(
        &self,
        house: &HouseId,
        evidence: &ScheduleEvidence,
        reported: impl Fn(&MarkerKey) -> bool,
    ) -> Result<Vec<BudgetExhaustion>, BudgetError> {
        let assessment = self.assess(house, evidence)?;
        let mut plan = Vec::new();
        for schedule in assessment.schedules {
            let Some(exhausted) = schedule.exhausted else {
                continue;
            };
            let pause = match schedule.state {
                ObservedScheduleState::Missing => continue,
                ObservedScheduleState::Paused => false,
                ObservedScheduleState::Active | ObservedScheduleState::Unknown => true,
            };
            let mut item = BudgetExhaustion {
                consumer: schedule.consumer,
                schedule: schedule.schedule,
                window: assessment.window,
                exhausted,
                pause,
                report_due: true,
            };
            item.report_due =
                !(reported(&item.marker_key()?) || reported(&item.undeliverable_key()?));
            if item.pause || item.report_due {
                plan.push(item);
            }
        }
        Ok(plan)
    }

    /// Schedules whose window the observed runs do not cover and whose run
    /// budget is not shown exhausted. The observed count is a lower bound, so
    /// the budget cannot be shown to hold.
    ///
    /// # Errors
    /// Any [`Self::assess`] refusal.
    pub fn unverifiable_run_budgets(
        &self,
        house: &HouseId,
        evidence: &ScheduleEvidence,
    ) -> Result<Vec<ScheduleAssessment>, BudgetError> {
        Ok(self
            .assess(house, evidence)?
            .schedules
            .into_iter()
            .filter(|schedule| !schedule.usage.complete && schedule.exhausted.is_none())
            .collect())
    }

    /// Schedules whose precheck reported idle for at least the policy's share
    /// of their recent runs, as recommendations to lengthen the interval or
    /// sharpen the precheck. Nothing is changed.
    #[must_use]
    pub fn idle_schedules(&self, evidence: &ScheduleEvidence) -> Vec<IdleSchedule> {
        evidence
            .schedules
            .iter()
            .filter_map(|schedule| {
                let runs = &schedule.observation.recent_runs;
                let total = u32::try_from(runs.len()).unwrap_or(u32::MAX);
                let idle = u32::try_from(
                    runs.iter()
                        .filter(|run| run.verdict == RunVerdict::Idle)
                        .count(),
                )
                .unwrap_or(u32::MAX);
                let share = u64::from(idle) * 100;
                let threshold = u64::from(total) * u64::from(self.idle.share_percent.get());
                (total >= self.idle.min_runs.get() && share >= threshold).then(|| IdleSchedule {
                    consumer: schedule.consumer.clone(),
                    schedule: schedule.schedule.clone(),
                    runs: total,
                    idle_runs: idle,
                    tokens: TokenUsage::of(runs.iter()),
                })
            })
            .collect()
    }

    /// Schedules with a token budget whose usage was unknown for more than
    /// half of their agent runs in the current window. Their token budget
    /// cannot be enforced; the run budget is the effective limit.
    ///
    /// # Errors
    /// Any [`Self::assess`] refusal.
    pub fn unenforceable_token_budgets(
        &self,
        house: &HouseId,
        evidence: &ScheduleEvidence,
    ) -> Result<Vec<ScheduleAssessment>, BudgetError> {
        let assessment = self.assess(house, evidence)?;
        Ok(assessment
            .schedules
            .into_iter()
            .filter(|schedule| {
                let TokenUsage::Unknown { unknown_runs, .. } = schedule.usage.tokens else {
                    return false;
                };
                self.budget_for(&schedule.consumer).tokens.is_some()
                    && u64::from(unknown_runs) * 2 > u64::from(schedule.usage.runs)
            })
            .collect())
    }
}

fn exhaustion(
    budget: &Budget,
    usage: &WindowUsage,
    runs_limit: ScheduleLimit,
    tokens_limit: ScheduleLimit,
) -> Option<Exhausted> {
    if usage.runs >= budget.runs.get() {
        return Some(Exhausted {
            limit: runs_limit,
            used: usage.runs.into(),
            allowed: budget.runs.get().into(),
        });
    }
    let allowed = budget.tokens?;
    let known = usage.tokens.known();
    (known >= allowed.get()).then_some(Exhausted {
        limit: tokens_limit,
        used: known,
        allowed: allowed.get(),
    })
}

/// Observed runs of the house's schedules, as evidence for budgets and doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleEvidence {
    /// The house whose schedules were observed.
    pub house: HouseId,
    /// When they were observed; selects the current window.
    pub observed_at: Timestamp,
    /// Each observed schedule.
    pub schedules: Vec<ScheduleUsage>,
}

impl ScheduleEvidence {
    /// Check bounds, and that no schedule appears twice: a duplicate would
    /// count its runs twice against the house budget.
    ///
    /// # Errors
    /// [`BudgetError::InvalidEvidence`] for more than
    /// [`MAX_EVIDENCE_SCHEDULES`] schedules, more than [`MAX_SCHEDULE_RUNS`]
    /// runs for one, or a repeated consumer or schedule.
    pub fn validate(&self) -> Result<(), BudgetError> {
        let consumers: BTreeSet<&ConsumerId> = self
            .schedules
            .iter()
            .map(|schedule| &schedule.consumer)
            .collect();
        let resources: BTreeSet<&ResourceRef> = self
            .schedules
            .iter()
            .map(|schedule| &schedule.schedule)
            .collect();
        let valid = self.schedules.len() <= MAX_EVIDENCE_SCHEDULES
            && consumers.len() == self.schedules.len()
            && resources.len() == self.schedules.len()
            && self
                .schedules
                .iter()
                .all(|schedule| schedule.observation.recent_runs.len() <= MAX_SCHEDULE_RUNS);
        if valid {
            Ok(())
        } else {
            Err(BudgetError::InvalidEvidence)
        }
    }
}

/// One schedule's observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleUsage {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// The backend resource.
    pub schedule: ResourceRef,
    /// Its state and recent runs.
    pub observation: ScheduleObservation,
}

/// Tokens used, as far as the backend reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TokenUsage {
    /// Every agent run reported its usage.
    Known {
        /// Tokens used.
        tokens: u64,
    },
    /// Some agent runs did not report usage. Their cost is unknown, not zero.
    Unknown {
        /// Tokens the reporting runs used; a lower bound.
        known_tokens: u64,
        /// Agent runs without reported usage.
        unknown_runs: u32,
    },
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self::Known { tokens: 0 }
    }
}

impl TokenUsage {
    /// The reported tokens: the total when known, a lower bound otherwise.
    #[must_use]
    pub const fn known(&self) -> u64 {
        match self {
            Self::Known { tokens } => *tokens,
            Self::Unknown { known_tokens, .. } => *known_tokens,
        }
    }

    fn add(self, tokens: u64, unknown: u32) -> Self {
        let known_tokens = self.known().saturating_add(tokens);
        let unknown_runs = match self {
            Self::Known { .. } => unknown,
            Self::Unknown { unknown_runs, .. } => unknown_runs.saturating_add(unknown),
        };
        if unknown_runs == 0 {
            Self::Known {
                tokens: known_tokens,
            }
        } else {
            Self::Unknown {
                known_tokens,
                unknown_runs,
            }
        }
    }

    fn of<'a>(runs: impl Iterator<Item = &'a JudgedRun>) -> Self {
        runs.fold(Self::default(), |usage, run| {
            let (tokens, unknown) = run_cost(run);
            usage.add(tokens, unknown)
        })
    }
}

/// Whether a run started, or may have started, an agent. A precheck that
/// reported idle or failed, and a skip before launch, start none.
const fn started_agent(verdict: RunVerdict) -> bool {
    match verdict {
        RunVerdict::Pending
        | RunVerdict::Started
        | RunVerdict::LaunchFailed
        | RunVerdict::Unknown => true,
        RunVerdict::Idle | RunVerdict::PrecheckFailed | RunVerdict::Skipped => false,
    }
}

/// A run's reported tokens, and whether its cost is unknown. Only a run that
/// started an agent can have an unknown cost; reported tokens always count.
fn run_cost(run: &JudgedRun) -> (u64, u32) {
    match &run.run.usage {
        Measurement::Observed { value, .. } => (*value, 0),
        Measurement::Missing | Measurement::Untested | Measurement::Unavailable => {
            (0, u32::from(started_agent(run.verdict)))
        }
    }
}

/// When a run belongs to: its due time, else when the backend recorded it.
fn placed_at(run: &ScheduleRun) -> Option<Timestamp> {
    run.scheduled_for.or(run.created_at)
}

/// Usage in one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindowUsage {
    /// Agent runs in the window.
    pub runs: u32,
    /// Tokens used in the window.
    pub tokens: TokenUsage,
    /// Whether the observation reached back to the window's start. When it
    /// did not, the counts are lower bounds.
    pub complete: bool,
}

impl WindowUsage {
    /// Usage in `window` among `observation`'s runs. A run with no due time,
    /// such as a trial, is placed by when the backend recorded it. With
    /// neither time it cannot be placed and counts in the window being
    /// judged, so an unplaceable run never hides usage.
    #[must_use]
    pub fn of(observation: &ScheduleObservation, window: UsageWindow) -> Self {
        let in_window: Vec<&JudgedRun> = observation
            .recent_runs
            .iter()
            .filter(|run| placed_at(&run.run).is_none_or(|at| window.contains(at)))
            .collect();
        let runs = u32::try_from(
            in_window
                .iter()
                .filter(|run| started_agent(run.verdict))
                .count(),
        )
        .unwrap_or(u32::MAX);
        let reached_start = observation
            .recent_runs
            .iter()
            .filter_map(|run| placed_at(&run.run))
            .min()
            .is_some_and(|oldest| oldest < window.start);
        Self {
            runs,
            tokens: TokenUsage::of(in_window.into_iter()),
            complete: observation.recent_runs.len() < MAX_SCHEDULE_RUNS || reached_start,
        }
    }

    fn plus(self, other: &Self) -> Self {
        let unknown = match other.tokens {
            TokenUsage::Known { .. } => 0,
            TokenUsage::Unknown { unknown_runs, .. } => unknown_runs,
        };
        Self {
            runs: self.runs.saturating_add(other.runs),
            tokens: self.tokens.add(other.tokens.known(), unknown),
            complete: self.complete && other.complete,
        }
    }
}

/// A budget found exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Exhausted {
    /// The exhausted limit.
    pub limit: ScheduleLimit,
    /// Usage observed in the window.
    pub used: u64,
    /// The budget.
    pub allowed: u64,
}

/// One schedule's usage in the current window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleAssessment {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// The backend resource.
    pub schedule: ResourceRef,
    /// Its observed state.
    pub state: ObservedScheduleState,
    /// Its usage in the window.
    pub usage: WindowUsage,
    /// The first exhausted budget that applies to it: its own, else the house's.
    pub exhausted: Option<Exhausted>,
}

/// Usage of a house's schedules in the current window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetAssessment {
    /// The window judged.
    pub window: UsageWindow,
    /// The house's total usage.
    pub house: WindowUsage,
    /// Whether the house budget is exhausted.
    pub house_exhausted: Option<Exhausted>,
    /// Each schedule.
    pub schedules: Vec<ScheduleAssessment>,
}

/// An exhausted schedule to pause and report to its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExhaustion {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// The backend resource.
    pub schedule: ResourceRef,
    /// The window its budget ran out in.
    pub window: UsageWindow,
    /// The exhausted limit and usage.
    pub exhausted: Exhausted,
    /// Whether it must be paused; `false` when it already is.
    pub pause: bool,
    /// Whether the owner has not yet been told about this window's
    /// exhaustion. Independent of `pause`.
    pub report_due: bool,
}

/// The fact recorded once a budget exhaustion was reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExhaustionReport {
    /// The window.
    pub window: UsageWindow,
    /// The exhausted limit and usage.
    pub exhausted: Exhausted,
}

/// An exhausted budget whose owner report had no destination, recorded once
/// per schedule and window. Doctor shows it until a report channel exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UndeliveredReport {
    /// The schedule that was paused.
    pub consumer: ConsumerId,
    /// The window its budget ran out in.
    pub window: UsageWindow,
    /// The exhausted limit and usage.
    pub exhausted: Exhausted,
}

/// The schema of the fact recorded under
/// [`BudgetExhaustion::undeliverable_key`].
///
/// # Errors
/// [`BudgetError::InvalidPolicy`] if the schema cannot be built.
pub fn undeliverable_schema() -> Result<MarkerSchema, BudgetError> {
    MarkerSchema::new("schedule-budget-undeliverable", NonZeroU32::MIN)
        .map_err(|_| BudgetError::InvalidPolicy)
}

impl BudgetExhaustion {
    /// The effect that pauses the schedule, when it must be paused.
    #[must_use]
    pub fn pause_effect(&self) -> Option<Effect> {
        self.pause.then(|| {
            Effect::Schedule(ScheduleEffect::SetState {
                schedule: self.schedule.clone(),
                state: ScheduleState::Paused,
            })
        })
    }

    /// The key a report of this exhaustion is recorded under: one per
    /// schedule and window.
    ///
    /// # Errors
    /// [`BudgetError::InvalidPolicy`] if a key part cannot be built.
    pub fn marker_key(&self) -> Result<MarkerKey, BudgetError> {
        self.key_for("")
    }

    /// The key recorded when the window's report had no destination: one per
    /// schedule and window, separate from [`Self::marker_key`], which stays
    /// absent because the owner was not told.
    ///
    /// # Errors
    /// [`BudgetError::InvalidPolicy`] if a key part cannot be built.
    pub fn undeliverable_key(&self) -> Result<MarkerKey, BudgetError> {
        self.key_for("-undeliverable")
    }

    fn key_for(&self, suffix: &str) -> Result<MarkerKey, BudgetError> {
        Ok(MarkerKey {
            workflow: WorkflowId::new(BUDGET_WORKFLOW).map_err(|_| BudgetError::InvalidPolicy)?,
            item: WorkItem::Resource {
                resource: self.schedule.clone(),
            },
            subject: MarkerSubject::Observation(
                ExternalRef::new(&format!(
                    "window-{}{suffix}",
                    self.window.start.as_unix_millis()
                ))
                .map_err(|_| BudgetError::InvalidPolicy)?,
            ),
        })
    }

    /// The fact to record under [`Self::marker_key`] once reported.
    ///
    /// # Errors
    /// [`BudgetError::InvalidPolicy`] if the fact cannot be encoded.
    pub fn marker_fact(&self) -> Result<MarkerFact, BudgetError> {
        let invalid = |_| BudgetError::InvalidPolicy;
        let schema =
            MarkerSchema::new("schedule-budget-exhausted", NonZeroU32::MIN).map_err(invalid)?;
        MarkerFact::workflow(
            schema,
            &ExhaustionReport {
                window: self.window,
                exhausted: self.exhausted,
            },
        )
        .map_err(invalid)
    }

    /// The fact to record under [`Self::undeliverable_key`].
    ///
    /// # Errors
    /// [`BudgetError::InvalidPolicy`] if the fact cannot be encoded.
    pub fn undeliverable_fact(&self) -> Result<MarkerFact, BudgetError> {
        let invalid = |_| BudgetError::InvalidPolicy;
        MarkerFact::workflow(undeliverable_schema()?, &self.undelivered()).map_err(invalid)
    }

    /// This exhaustion as an [`UndeliveredReport`].
    #[must_use]
    pub fn undelivered(&self) -> UndeliveredReport {
        UndeliveredReport {
            consumer: self.consumer.clone(),
            window: self.window,
            exhausted: self.exhausted,
        }
    }

    /// The owner-facing report: which schedule stopped, the exhausted limit,
    /// and what resuming takes. The text leaves out usage, which can grow
    /// between passes, so a report retried later is the same report.
    #[must_use]
    pub fn report(&self) -> String {
        format!(
            "Paused schedule {}: the {} of {} is exhausted for the window ending at {} (Unix ms). It stays paused until the owner activates it, which is refused until that window ends or the budget is raised.",
            self.consumer,
            self.exhausted.limit,
            self.exhausted.allowed,
            self.window.end.as_unix_millis(),
        )
    }
}

/// A schedule whose precheck mostly reports idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdleSchedule {
    /// The consumer scope it serves.
    pub consumer: ConsumerId,
    /// The backend resource.
    pub schedule: ResourceRef,
    /// Recent runs observed.
    pub runs: u32,
    /// Of those, runs whose precheck reported idle.
    pub idle_runs: u32,
    /// Tokens those runs used.
    pub tokens: TokenUsage,
}

/// The shortest gap in minutes between two runs of `recurrence`, in its
/// wall-clock time; `None` when it fires at most once.
///
/// Cron fields follow the common convention: `a-b`, `*/n`, `a/n` (from `a`
/// to the field's end), lists, and day of week 0–7 with 0 and 7 both
/// Sunday. When both day of month and day of week are restricted (neither
/// starts with `*`), a day matches either one. Days are evaluated over a
/// 28-year calendar cycle, which repeats weekdays and leap days.
///
/// # Errors
/// [`BudgetError::UnreadableRecurrence`] for fields out of range.
pub fn shortest_interval_minutes(recurrence: &Recurrence) -> Result<Option<u32>, BudgetError> {
    let cron = recurrence.cron();
    let fields = CronFields::parse(&cron).ok_or(BudgetError::UnreadableRecurrence)?;
    let times: Vec<u32> = (0..24_u32)
        .filter(|hour| fields.hours & (1 << hour) != 0)
        .flat_map(|hour| {
            (0..60_u32)
                .filter(|minute| fields.minutes & (1 << minute) != 0)
                .map(move |minute| hour * 60 + minute)
        })
        .collect();
    let (Some(&first), Some(&last)) = (times.first(), times.last()) else {
        return Ok(None);
    };
    let mut shortest = times
        .windows(2)
        .filter_map(|pair| match pair {
            [earlier, later] => Some(later - earlier),
            _ => None,
        })
        .min();
    let mut day = time::Date::from_calendar_date(2000, time::Month::January, 1)
        .map_err(|_| BudgetError::UnreadableRecurrence)?;
    let mut previous: Option<u32> = None;
    // 28 years of days, leap days included.
    for index in 0..10_227_u32 {
        if fields.matches(day) {
            if let Some(earlier) = previous {
                let gap = (index - earlier) * MINUTES_PER_DAY + first - last;
                shortest = Some(shortest.map_or(gap, |current| current.min(gap)));
            }
            previous = Some(index);
        }
        day = day.next_day().ok_or(BudgetError::UnreadableRecurrence)?;
    }
    if previous.is_none() {
        return Ok(None);
    }
    Ok(shortest)
}

/// Parsed cron fields as bit sets.
struct CronFields {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    either_day: bool,
}

impl CronFields {
    fn parse(cron: &str) -> Option<Self> {
        let fields: Vec<&str> = cron.split(' ').collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return None;
        };
        let mut weekdays = field_bits(weekday, 0, 7)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Some(Self {
            minutes: field_bits(minute, 0, 59)?,
            hours: field_bits(hour, 0, 23)?,
            days: field_bits(day, 1, 31)?,
            months: field_bits(month, 1, 12)?,
            weekdays,
            either_day: !day.starts_with('*') && !weekday.starts_with('*'),
        })
    }

    fn matches(&self, date: time::Date) -> bool {
        let month = self.months & (1 << u8::from(date.month())) != 0;
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().number_days_from_sunday()) != 0;
        month
            && if self.either_day {
                day || weekday
            } else {
                day && weekday
            }
    }
}

fn field_bits(field: &str, min: u8, max: u8) -> Option<u64> {
    field.split(',').try_fold(0_u64, |bits, item| {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, step.parse::<u8>().ok().filter(|step| *step > 0)?),
            None => (item, 1),
        };
        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            (low.parse().ok()?, high.parse().ok()?)
        } else {
            let value: u8 = range.parse().ok()?;
            (value, if item.contains('/') { max } else { value })
        };
        if low < min || high > max || low > high {
            return None;
        }
        Some(
            (low..=high)
                .step_by(usize::from(step))
                .fold(bits, |bits, value| bits | (1 << value)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{CronExpr, TimeOfDay, Weekday};

    fn gap(recurrence: &Recurrence) -> Option<u32> {
        shortest_interval_minutes(recurrence).ok().flatten()
    }

    fn cron(expr: &str) -> Result<Recurrence, Box<dyn std::error::Error>> {
        Ok(Recurrence::Cron(CronExpr::new(expr)?))
    }

    #[test]
    fn presets_have_their_natural_interval() -> Result<(), Box<dyn std::error::Error>> {
        let at = TimeOfDay::new(9, 30)?;
        assert_eq!(gap(&Recurrence::Hourly), Some(60));
        assert_eq!(gap(&Recurrence::Daily(at)), Some(MINUTES_PER_DAY));
        assert_eq!(gap(&Recurrence::Weekdays(at)), Some(MINUTES_PER_DAY));
        assert_eq!(
            gap(&Recurrence::Weekly(Weekday::Monday, at)),
            Some(7 * MINUTES_PER_DAY)
        );
        Ok(())
    }

    #[test]
    fn cron_gaps_take_the_shortest_including_across_midnight()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(gap(&cron("17,37,57 * * * *")?), Some(20));
        assert_eq!(gap(&cron("*/5 9-17 * * 1-5")?), Some(5));
        // 23:50 then 00:10 the next day is 20 minutes apart.
        assert_eq!(gap(&cron("10,50 0,23 * * *")?), Some(20));
        // Uneven lists: the shortest pair wins.
        assert_eq!(gap(&cron("0,5,30 * * * *")?), Some(5));
        // Monday and Friday at noon: Friday to Monday is shorter than a week.
        assert_eq!(gap(&cron("0 12 * * 1,5")?), Some(3 * MINUTES_PER_DAY));
        // Day 7 is Sunday, like day 0.
        assert_eq!(gap(&cron("0 0 * * 7")?), Some(7 * MINUTES_PER_DAY));
        Ok(())
    }

    #[test]
    fn restricted_day_fields_match_either_day() -> Result<(), Box<dyn std::error::Error>> {
        // The 1st of the month or any Monday: a Monday the 31st is followed
        // by the 1st one day later.
        assert_eq!(gap(&cron("0 0 1 * 1")?), Some(MINUTES_PER_DAY));
        // February 29th only: four years apart.
        assert_eq!(
            gap(&cron("0 0 29 2 *")?),
            Some((365 * 3 + 366) * MINUTES_PER_DAY)
        );
        Ok(())
    }

    #[test]
    fn schedules_that_never_fire_have_no_interval() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(gap(&cron("0 0 31 2 *")?), None, "never fires");
        Ok(())
    }

    #[test]
    fn out_of_range_fields_are_unreadable() -> Result<(), Box<dyn std::error::Error>> {
        for expr in [
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "*/0 * * * *",
            "1-2-3 * * * *",
        ] {
            assert_eq!(
                shortest_interval_minutes(&cron(expr)?),
                Err(BudgetError::UnreadableRecurrence),
                "{expr}"
            );
        }
        Ok(())
    }

    #[test]
    fn windows_are_fixed_and_aligned_to_the_epoch() -> Result<(), Box<dyn std::error::Error>> {
        let day = WindowHours::new(24)?;
        let hour = 3_600_000;
        let window = day.containing(Timestamp::from_unix_millis(30 * hour));
        assert_eq!(window.start, Timestamp::from_unix_millis(24 * hour));
        assert_eq!(window.end, Timestamp::from_unix_millis(48 * hour));
        assert!(window.contains(Timestamp::from_unix_millis(24 * hour)));
        assert!(!window.contains(Timestamp::from_unix_millis(48 * hour)));
        assert_eq!(WindowHours::new(0), Err(BudgetError::InvalidPolicy));
        assert_eq!(
            WindowHours::new(MAX_WINDOW_HOURS + 1),
            Err(BudgetError::InvalidPolicy)
        );
        Ok(())
    }
}
