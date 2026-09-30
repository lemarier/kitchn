//! GitHub access constrained by an explicit house selection.
//!
//! Configuration supplies [`HouseScope`], an allowlist, requester identity,
//! core credential ID, permitted effects, and a per-task logical posting ceiling.
//! It never supplies secret values through a workflow payload. [`CredentialFile`]
//! resolves the private token only at the CLI boundary; [`GhCli`] clears ambient
//! CLI configuration and verifies the authenticated GitHub login before each call.
//! A token that cannot authenticate `/user` is explicitly unavailable. A GitHub
//! App credential ([`GhCli::app`]) instead runs each effect's calls with an
//! installation token that [`AppTokens`] mints for that effect's repository and
//! [`TokenScope`], verified through its installation.
//!
//! Build a [`GitHubExecutor::effect`], put it in a [`crate::state::EffectPlan`],
//! and call [`crate::state::run_effect`]. The core owns claims, current authority,
//! revision checks, atomic posting budgets, and intent durability. After an
//! uncertain result call [`crate::state::reconcile`]; never submit it directly
//! again. GitHub offers no native idempotency guarantee, so an absent marker is
//! inconclusive and cannot authorize another post. Receipts for created issues
//! and comments contain their forge URL; other receipts identify the intent.
//!
//! Setup first previews [`LabelDefinition::inspect`]. Only missing labels need
//! intents, and execution rechecks the inventory. Conflicting existing labels
//! are refused without renaming, recoloring, or deleting them. Budgets count
//! distinct admitted effects conservatively, including unresolved or no-op
//! effects, rather than promising a wall-clock posting rate limit.
//! Provider 4xx refusals are definitely not applied; rate-limit responses carry
//! a typed retry delay when one is supplied. Transport and 5xx outcomes require
//! reconciliation. The executor admits and executes a merge only at a subject
//! a readiness-checked [`MergeGrant`](crate::workflows::gate::MergeGrant) given
//! to [`GitHubExecutor::with_merge_grant`] covers, so a house below its merge
//! readiness policy cannot merge through it. A merge must also name the task's
//! current evidence subject: core
//! refuses it at admission unless `expected_head` and `expected_base_commit`
//! equal the recorded head and base, so callers record evidence before
//! merging. The provider then requires the approved head and base branch; a
//! retargeted or moved pull request is refused before submission, but the base
//! commit is not re-read. A merge at the approved head by another actor or
//! method reads back as applied.
//! `SetLabel` refuses a label the repository does not define. `CloseIssue`
//! requires the distinct [`Permission::CloseIssue`](crate::contracts::Permission)
//! grant; its PATCH field `duplicate_issue_id` and GraphQL `duplicateOf`
//! read-back are not yet verified against the live API, and a wrong shape
//! reads back as unknown or conflicting, never as applied.
//! `OpenPullRequest` requires [`Permission::OpenPullRequest`](crate::contracts::Permission)
//! and never pushes: it lists the head branch's pull requests and reads the
//! remote branch, and opens one only when the requester's marker is absent,
//! no other open pull request comes from that head, and the
//! remote branch holds `expected_head`. A marked pull request reads back as
//! applied with its URL, even after its head moved or its base changed, so
//! reconciling an uncertain open finds it instead of opening another.

mod app;
mod mutation;
pub(crate) mod process;
mod scope;
pub use crate::contracts::{
    CloseReason, GitHubAction, GitHubMutation, IssueNumber, LabelDefinition, MergeMethod,
    PostingBudget, ReviewVerdict,
};
pub use app::{
    Access, AppApi, AppAuth, AppId, AppPermission, AppRequest, AppResponse, AppTokens, CurlApi,
    GitHubApp, InstallationId, Installed, REFRESH_MARGIN, TokenScope,
};
pub use mutation::LabelSetup;
pub use process::{CredentialFile, GhCli};
mod client;
mod evidence;
pub use client::{
    GitHubClient, GitHubReadTransport, MAX_PULL_REQUEST_COMMITS, ReadLimits, ReadRequest,
};
pub use evidence::*;

pub use scope::{CredentialRef, HouseScope};

use crate::ErrorClass;

/// Integration failures never contain credential values or response bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    /// Invalid bounded input or provider response.
    #[error("invalid integration input")]
    InvalidInput,
    /// The selected house, requester, credential, or destination disagrees.
    #[error("integration scope mismatch")]
    ScopeMismatch,
    /// The house does not permit this effect.
    #[error("integration effect is not permitted")]
    PermissionDenied,
    /// The selected forge policy lacks this permission.
    #[error("forge policy does not permit {0}")]
    MissingPermission(crate::contracts::Permission),
    /// The task grant names a different forge credential.
    #[error("task grant credential differs from the forge binding for {0}")]
    CredentialMismatch(crate::contracts::Permission),
    /// The provider refused an HTTP request; no response body is exposed.
    #[error("GitHub HTTP {0}")]
    HttpStatus(u16),
    /// A worker delivery has no running attempt under its current claim.
    #[error("worker delivery requires a running attempt")]
    AttemptNotRunning,
    /// The worker could not be positively observed as live.
    #[error("worker delivery requires a live worker observation")]
    WorkerNotLive,
    /// The backend could not establish the worker's state.
    #[error("worker delivery cannot observe the worker")]
    WorkerUnobservable,
    /// A checked push failed a local worktree or claim preflight.
    #[error("push preflight failed: {0}")]
    PushPreflight(PushPreflight),
    /// Worker pushes need an installation token scoped to one repository.
    #[error(
        "worker push requires a GitHub App forge binding; use `kitchn forge bind --app-id ... --installation ...`"
    )]
    AppRequiredForPush,
    /// All permitted submissions for this durable task have been spent.
    #[error("posting budget exhausted")]
    BudgetExhausted,
    /// A bounded operation timed out.
    #[error("integration deadline exceeded")]
    Timeout,
    /// The provider could not be reached or returned an error.
    #[error("integration unavailable")]
    Unavailable,
    /// The provider answered that the requested resource does not exist.
    #[error("integration resource not found")]
    NotFound,
    /// Input/output or pagination reached a configured bound.
    #[error("integration resource bound exceeded")]
    LimitExceeded,
    /// A response was malformed or ambiguous.
    #[error("integration response is unknown")]
    Unknown,
    /// The answer refers to an obsolete revision.
    #[error("decision revision is stale")]
    StaleDecision,
}

impl IntegrationError {
    /// Broad handling class used at executable boundaries.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::InvalidInput => ErrorClass::InvalidInput,
            Self::ScopeMismatch
            | Self::PermissionDenied
            | Self::MissingPermission(_)
            | Self::CredentialMismatch(_)
            | Self::HttpStatus(401 | 403)
            | Self::AttemptNotRunning
            | Self::WorkerNotLive
            | Self::WorkerUnobservable
            | Self::PushPreflight(_)
            | Self::AppRequiredForPush
            | Self::BudgetExhausted => ErrorClass::Refused,
            Self::StaleDecision => ErrorClass::Conflict,
            Self::HttpStatus(_)
            | Self::Timeout
            | Self::Unavailable
            | Self::NotFound
            | Self::LimitExceeded
            | Self::Unknown => ErrorClass::Execution,
        }
    }
}

/// Which local fact failed before a checked worker push could begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PushPreflight {
    /// The task has no live uncancelled claim.
    #[error("task claim is absent, expired, or cancelled")]
    Claim,
    /// The current launch has no worker receipt.
    #[error("current worker launch is absent")]
    Worker,
    /// Orca did not identify the invoking worktree.
    #[error("Orca did not identify the invoking worktree")]
    WorktreeContext,
    /// The invocation is outside Orca's worktree path.
    #[error("invocation is outside the recorded worktree")]
    WorktreePath,
    /// The worktree is not the current launch's worktree.
    #[error("worktree does not match the current launch")]
    WorktreeOwnership,
    /// Orca associated the worktree with another repository.
    #[error("worktree repository does not match the task")]
    WorktreeRepository,
    /// The task has no launched branch.
    #[error("current launch has no branch")]
    Branch,
    /// The Git and Orca checkouts disagree with the launch record.
    #[error("checkout branch or head differs from the current launch")]
    Checkout,
    /// The acceptance report resolves outside the launched worktree.
    #[error("acceptance report resolves outside the worktree")]
    AcceptanceReport,
}

impl From<crate::contracts::ContractError> for IntegrationError {
    fn from(error: crate::contracts::ContractError) -> Self {
        match error.class() {
            ErrorClass::InvalidInput => Self::InvalidInput,
            ErrorClass::Refused => Self::ScopeMismatch,
            ErrorClass::Conflict => Self::StaleDecision,
            ErrorClass::Execution => Self::Unknown,
        }
    }
}
mod executor;
mod provider;
pub use executor::GitHubExecutor;
pub use provider::{GitHubMutationTransport, MutationRequest};
