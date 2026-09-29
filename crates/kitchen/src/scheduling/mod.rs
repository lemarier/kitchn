//! Orchestrator-neutral schedule definitions and install reconciliation.
//!
//! A [`ScheduleSpec`] describes a recurring workflow without naming any
//! orchestrator: triggers, prechecks, and workspaces are typed values, and an
//! adapter renders them into its own format. A precheck is an argument vector,
//! never a shell string; adapters that need a shell must quote it themselves.
//!
//! Installs are never assumed idempotent. Orca 1.4.212's `automations
//! create`, `edit`, `remove`, and `run` accept no request key, so a retried
//! install could create a second consumer. Before any create or retry, callers
//! reconcile against the installed inventory with [`plan_install`]; a lost
//! response is resolved by that lookup, and anything it cannot establish stays
//! unknown rather than becoming a second create.
//!
//! A backend's record that a launch finished is not evidence that the agent
//! started. [`Readiness`] joins each run with Kitchen's own
//! [`ReadinessSignal`]s and a deadline into a [`RunVerdict`], and adapters
//! return runs already judged, so a swallowed launch is a
//! [`RunVerdict::LaunchFailed`] instead of a completed run.
//!
//! A house's [`SchedulePolicy`] limits how often schedules fire and how much
//! they spend per window; installs that break it are refused, and exhausted
//! budgets pause the schedule and are reported once.
//!
//! [`crate::contracts::ScheduleEffect`] carries these types in persisted
//! effect intents.

mod budget;
mod reconcile;
mod spec;

pub use budget::{
    BUDGET_WORKFLOW, Budget, BudgetAssessment, BudgetError, BudgetExhaustion, Exhausted,
    ExhaustionReport, IdlePolicy, IdleSchedule, IntervalMinutes, MAX_EVIDENCE_SCHEDULES,
    MAX_INTERVAL_MINUTES, MAX_SCHEDULE_LIMITS, MAX_WINDOW_HOURS, Percent, ScheduleAssessment,
    ScheduleEvidence, ScheduleLimit, ScheduleLimits, SchedulePolicy, ScheduleUsage, TokenUsage,
    UndeliveredReport, UsageWindow, WindowHours, WindowUsage, exhausted_schema,
    shortest_interval_minutes, undeliverable_schema,
};
pub use reconcile::{
    InstallPlan, InstalledSchedule, JudgedRun, MAX_SCHEDULE_RUNS, ObservedScheduleState, Readiness,
    ReadinessSignal, RunOutcome, RunVerdict, ScheduleField, ScheduleObservation, ScheduleRun,
    plan_install, run_verdict,
};
pub use spec::{
    AgentFamily, CronExpr, GraceMinutes, MAX_PRECHECK_ARGS, Precheck, PrecheckOutcome,
    PrecheckTimeout, Recurrence, ScheduleError, ScheduleSpec, ScheduleState, ScheduleWorkspace,
    TimeOfDay, Timezone, Weekday, WorkflowName,
};
