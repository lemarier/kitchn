//! Project decomposition: turn an epic or rough idea into dependency-linked
//! sub-issues that pickup can run, written only after the owner approves the
//! exact preview.
//!
//! A [`Proposal`] names each issue with its outcome, owned paths, acceptance
//! criteria, and blocked-by edges. [`preview`] only reads it: it rejects
//! dependency cycles, orders issues so every blocker comes first, flags
//! proposed issues whose owned paths overlap, counts the forge writes, and
//! binds all of it to one [`PreviewDigest`]. Overlapping issues that no
//! blocked-by path orders make the preview not ready; they must be merged or
//! ordered first.
//!
//! [`apply`] recomputes the preview from the proposal it is given and writes
//! nothing unless the person's [`Approval`] names that exact digest, so a
//! changed proposal needs a new approval. Only an interactive claimant can
//! apply: the approval is a person's consent, and each forge write carries a
//! [`Consent`] derived from it for exactly that write. The library cannot tell
//! a person from a script, so callers must build an [`Approval`] only from
//! what a person present agreed to. Scheduled workflows never build one.
//!
//! All writes of one preview run as one durable Kitchen task whose id comes
//! from the digest, through [`run_effect`] and the GitHub executor: issues in
//! preview order, then sub-issue links to the parent, then blocked-by links.
//! Each write has a logical name. A retry after partial creation reconciles
//! uncertain writes first, reuses the receipt of every write already applied
//! in any earlier attempt, and submits only the rest, so it completes the set
//! without duplicate issues or edges. A write whose outcome stays unknown
//! stops the run: nothing after it is submitted until it is reconciled.
//! A second decomposition of the same repository is refused while an earlier
//! one is unfinished and either is being run or has a write that may have
//! reached the forge, so a revised proposal cannot recreate issues that the
//! earlier task created. The check, the creation of the task, and its claim
//! are one store transaction ([`HouseStore::reserve_task`]), so two approved
//! previews cannot both pass. The slot frees when the earlier task settles,
//! or when its claim lapses with no write that could have reached the forge.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    EffectName, Error, ErrorClass, HolderId, Result, TaskId,
    contracts::{
        AttemptNumber, AttemptOutcome, AttemptStart, CapabilityRequirements, Claimant, Clock,
        Consent, Effect, ExternalRef, FailureClass, GitHubAction, GitHubMutation, HouseGrants,
        IssueNumber, LeaseTtl, NotAppliedReason, Provenance, Repository, RetryPolicy, Role,
        Settlement, TaskAuthority, TaskSpec, Text, Timestamp, Trigger,
    },
    integrations::github::{GitHubExecutor, GitHubMutationTransport},
    state::{
        EffectPlan, EffectRecord, EffectState, HouseStore, Reservation, StateError, TaskRecord,
        TaskState, reconcile, run_effect,
    },
};

/// The workflow id of decomposition.
pub const WORKFLOW: &str = "decomposition";
/// Prefix of the tasks decomposition creates, one per approved preview.
pub const TASK_PREFIX: &str = "decompose-";
/// Most issues one proposal may create.
pub const MAX_ISSUES: usize = 30;
/// Most blocked-by edges one proposed issue may name.
pub const MAX_BLOCKERS: usize = 10;
/// Most owned paths one proposed issue may name.
pub const MAX_OWNED_PATHS: usize = 20;
/// Most acceptance criteria one proposed issue may name.
pub const MAX_CRITERIA: usize = 20;
/// Longest issue key in bytes; it is part of every write's logical name.
pub const MAX_KEY_BYTES: usize = 24;
const MAX_TITLE_BYTES: usize = 200;
const MAX_PHASE_BYTES: usize = 80;
const MAX_OUTCOME_BYTES: usize = 4096;
const MAX_CRITERION_BYTES: usize = 500;
const MAX_PATH_BYTES: usize = 200;
/// Attempts one decomposition task may use, including recovery.
const ATTEMPTS: u32 = 4;
/// Longest a decomposition task may keep retrying after its first attempt.
const BUDGET: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Why a proposal or an apply call was refused. Private issue text is never
/// included; issue keys are the proposal's own identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecompositionError {
    /// A field is empty, too long, holds control characters, or a list is
    /// over its bound.
    #[error("proposal field `{field}` is missing, too long, or malformed")]
    InvalidField {
        /// The offending field.
        field: &'static str,
    },
    /// The proposal names no issue or more than [`MAX_ISSUES`].
    #[error("a proposal must name 1 to {MAX_ISSUES} issues")]
    IssueCount,
    /// Two proposed issues share a key.
    #[error("issue key `{0}` is used more than once")]
    DuplicateKey(IssueKey),
    /// A blocked-by edge names a key the proposal does not define.
    #[error("issue `{issue}` is blocked by unknown issue `{blocker}`")]
    UnknownBlocker {
        /// The blocked issue.
        issue: IssueKey,
        /// The key that does not exist.
        blocker: IssueKey,
    },
    /// An issue names itself or the same blocker twice.
    #[error("issue `{0}` repeats a blocker or blocks itself")]
    InvalidBlocker(IssueKey),
    /// The blocked-by edges form a cycle. Each key is blocked by the next,
    /// and the last repeats the first.
    #[error("dependency cycle: {}", render_cycle(.0))]
    Cycle(Vec<IssueKey>),
    /// Only a person present can approve and apply a decomposition.
    #[error("a decomposition must be applied by an interactive claimant")]
    ApprovalNeedsPerson,
    /// A created issue's receipt does not name an issue in the repository.
    #[error("the forge receipt of a created issue names no issue in the repository")]
    UnreadableReceipt,
    /// The preview could not be encoded for its digest.
    #[error("the preview could not be encoded")]
    Encoding,
}

impl DecompositionError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidField { .. }
            | Self::IssueCount
            | Self::DuplicateKey(_)
            | Self::UnknownBlocker { .. }
            | Self::InvalidBlocker(_)
            | Self::Cycle(_) => ErrorClass::InvalidInput,
            Self::ApprovalNeedsPerson => ErrorClass::Refused,
            Self::UnreadableReceipt | Self::Encoding => ErrorClass::Execution,
        }
    }
}

fn render_cycle(keys: &[IssueKey]) -> String {
    let mut out = String::new();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            out.push_str(" -> ");
        }
        out.push_str(key.as_str());
    }
    out
}

fn field(name: &'static str) -> Error {
    DecompositionError::InvalidField { field: name }.into()
}

/// A proposed issue's key: 1 to [`MAX_KEY_BYTES`] lowercase ASCII letters,
/// digits, or `-`, starting with a letter or digit. It names the issue in the
/// proposal and in the logical name of each write.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IssueKey(String);

impl IssueKey {
    /// Validate a key.
    ///
    /// # Errors
    /// [`DecompositionError::InvalidField`] for anything outside the syntax.
    pub fn new(value: &str) -> Result<Self> {
        let valid = !value.is_empty()
            && value.len() <= MAX_KEY_BYTES
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(field("key"));
        }
        Ok(Self(value.to_owned()))
    }

    /// The key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IssueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for IssueKey {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(&value)
    }
}

impl From<IssueKey> for String {
    fn from(value: IssueKey) -> Self {
        value.0
    }
}

/// A repository-relative path an issue owns: a file or a directory, written
/// with `/` separators, without `.` or `..` segments, a leading `/`, or
/// characters outside printable ASCII except space. A trailing `/` is
/// dropped. Two paths overlap when they are equal or one is a directory
/// containing the other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OwnedPath(String);

impl OwnedPath {
    /// Validate and normalize a path.
    ///
    /// # Errors
    /// [`DecompositionError::InvalidField`] for an empty, absolute, escaping,
    /// or malformed path.
    pub fn new(value: &str) -> Result<Self> {
        let trimmed = value.strip_suffix('/').unwrap_or(value);
        let valid = !trimmed.is_empty()
            && trimmed.len() <= MAX_PATH_BYTES
            && trimmed
                .bytes()
                .all(|byte| (b' '..=b'~').contains(&byte) && !matches!(byte, b'\\' | b'`'))
            && trimmed
                .split('/')
                .all(|segment| !matches!(segment, "" | "." | ".."));
        if !valid {
            return Err(field("ownedPaths"));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The normalized path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the two paths are equal or one contains the other.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        let contains = |outer: &str, inner: &str| {
            inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        contains(&self.0, &other.0) || contains(&other.0, &self.0)
    }
}

impl TryFrom<String> for OwnedPath {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(&value)
    }
}

impl From<OwnedPath> for String {
    fn from(value: OwnedPath) -> Self {
        value.0
    }
}

/// What blocks a proposed issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Blocker {
    /// Another issue of the same proposal, by key.
    Proposed(IssueKey),
    /// An issue that already exists in the repository.
    Existing(IssueNumber),
}

/// One issue the owner is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposedIssue {
    /// The issue's key within the proposal.
    pub key: IssueKey,
    /// The phase the issue belongs to, for presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Issue title.
    pub title: String,
    /// The outcome the issue delivers.
    pub outcome: String,
    /// Paths the issue owns; at least one.
    pub owned_paths: Vec<OwnedPath>,
    /// Acceptance criteria; at least one.
    pub acceptance: Vec<String>,
    /// Issues that must land first.
    #[serde(default)]
    pub blocked_by: Vec<Blocker>,
}

/// A proposed decomposition of one project into sub-issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Proposal {
    /// The repository the issues are created in.
    pub repository: Repository,
    /// The epic every created issue becomes a sub-issue of, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<IssueNumber>,
    /// The proposed issues.
    pub issues: Vec<ProposedIssue>,
}

/// The digest binding an approval to one exact preview: `sha256:` followed
/// by 64 lowercase hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PreviewDigest(String);

impl PreviewDigest {
    /// The digest text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

impl FromStr for PreviewDigest {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(field("digest"));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for PreviewDigest {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl From<PreviewDigest> for String {
    fn from(value: PreviewDigest) -> Self {
        value.0
    }
}

impl fmt::Display for PreviewDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One issue as it will be created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewIssue {
    /// The issue's key.
    pub key: IssueKey,
    /// Its phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The exact title that will be written.
    pub title: String,
    /// The exact body that will be written.
    pub body: String,
    /// Owned paths.
    pub owned_paths: Vec<OwnedPath>,
    /// Acceptance criteria.
    pub acceptance: Vec<String>,
    /// Blocked-by edges, recorded as forge relationships.
    pub blocked_by: Vec<Blocker>,
}

/// How an ownership overlap between two proposed issues is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OverlapResolution {
    /// A blocked-by path orders the two: `first` lands before the other.
    Ordered {
        /// The issue that lands first.
        first: IssueKey,
    },
    /// Nothing orders them. They must be merged or ordered before approval.
    Unordered,
}

/// Two proposed issues that own overlapping paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overlap {
    /// The earlier issue in preview order.
    pub left: IssueKey,
    /// The later issue in preview order.
    pub right: IssueKey,
    /// Every overlapping pair of paths, as (left's, right's).
    pub paths: Vec<(OwnedPath, OwnedPath)>,
    /// Whether a dependency orders them.
    pub resolution: OverlapResolution,
}

/// The forge writes a preview needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteCount {
    /// Issues to create.
    pub issues: u32,
    /// Sub-issue links to the parent.
    pub sub_issue_links: u32,
    /// Blocked-by links.
    pub dependencies: u32,
}

impl WriteCount {
    /// All writes.
    #[must_use]
    pub const fn total(self) -> u32 {
        self.issues
            .saturating_add(self.sub_issue_links)
            .saturating_add(self.dependencies)
    }
}

/// Everything the owner approves, bound to one digest. Issues are listed in
/// creation order: every blocker precedes the issues it blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// Destination repository.
    pub repository: Repository,
    /// The epic the issues become sub-issues of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<IssueNumber>,
    /// The issues in creation order.
    pub issues: Vec<PreviewIssue>,
    /// Proposed issues with overlapping owned paths.
    pub overlaps: Vec<Overlap>,
    /// The forge writes applying this preview needs.
    pub writes: WriteCount,
    /// The digest an approval must name.
    pub digest: PreviewDigest,
}

impl Preview {
    /// Whether the preview may be approved: no two issues own overlapping
    /// paths without a dependency ordering them.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.overlaps
            .iter()
            .all(|overlap| matches!(overlap.resolution, OverlapResolution::Ordered { .. }))
    }

    /// The preview as plain text for a person to read before approving.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Decomposition of {}", self.repository.as_str());
        if let Some(parent) = self.parent {
            let _ = writeln!(out, "Parent: #{}", parent.get());
        }
        for (index, issue) in self.issues.iter().enumerate() {
            let _ = writeln!(out, "\n{}. [{}] {}", index + 1, issue.key, issue.title);
            if let Some(phase) = &issue.phase {
                let _ = writeln!(out, "   Phase: {phase}");
            }
            if !issue.blocked_by.is_empty() {
                let blockers: Vec<String> = issue.blocked_by.iter().map(render_blocker).collect();
                let _ = writeln!(out, "   Blocked by: {}", blockers.join(", "));
            }
            // The body carries the outcome, owned paths, and acceptance
            // criteria; it is shown as posted so the digest covers what
            // the person read.
            let _ = writeln!(out, "----- body of #{} as posted -----", index + 1);
            let _ = write!(out, "{}", issue.body);
            if !issue.body.ends_with('\n') {
                out.push('\n');
            }
            let _ = writeln!(out, "----- end of body -----");
        }
        if !self.overlaps.is_empty() {
            let _ = writeln!(out, "\nOwnership overlaps:");
            for overlap in &self.overlaps {
                let state = match &overlap.resolution {
                    OverlapResolution::Ordered { first } => format!("ordered, {first} first"),
                    OverlapResolution::Unordered => "UNORDERED: merge or order them".into(),
                };
                let _ = writeln!(out, "- {} and {}: {state}", overlap.left, overlap.right);
            }
        }
        let _ = writeln!(
            out,
            "\nWrites: {} issues, {} sub-issue links, {} blocked-by links",
            self.writes.issues, self.writes.sub_issue_links, self.writes.dependencies
        );
        let _ = write!(
            out,
            "Digest: {}{}",
            self.digest,
            if self.ready() {
                ""
            } else {
                "\nNot ready: resolve the unordered overlaps first."
            }
        );
        out
    }

    fn position(&self, key: &IssueKey) -> Option<usize> {
        self.issues.iter().position(|issue| &issue.key == key)
    }
}

fn render_blocker(blocker: &Blocker) -> String {
    match blocker {
        Blocker::Proposed(key) => key.to_string(),
        Blocker::Existing(number) => format!("#{}", number.get()),
    }
}

/// Validate a proposal and build the preview the owner approves. Reads and
/// writes nothing.
///
/// # Errors
/// [`DecompositionError`] for a malformed field, an issue count or list over
/// its bound, a duplicate key, an unknown or repeated blocker, and a
/// dependency cycle.
pub fn preview(proposal: &Proposal) -> Result<Preview> {
    let count = proposal.issues.len();
    if count == 0 || count > MAX_ISSUES {
        return Err(DecompositionError::IssueCount.into());
    }
    let mut index = BTreeMap::new();
    for (position, issue) in proposal.issues.iter().enumerate() {
        validate_issue(issue)?;
        if index.insert(issue.key.clone(), position).is_some() {
            return Err(DecompositionError::DuplicateKey(issue.key.clone()).into());
        }
    }
    // Edges between proposed issues: `blockers[i]` are the positions that
    // must land before issue `i`.
    let mut blockers: Vec<Vec<usize>> = Vec::with_capacity(count);
    for issue in &proposal.issues {
        let mut local = Vec::new();
        for (position, blocker) in issue.blocked_by.iter().enumerate() {
            let repeated = issue
                .blocked_by
                .get(..position)
                .unwrap_or_default()
                .contains(blocker);
            if repeated || blocker == &Blocker::Proposed(issue.key.clone()) {
                return Err(DecompositionError::InvalidBlocker(issue.key.clone()).into());
            }
            if let Blocker::Proposed(key) = blocker {
                let position =
                    index
                        .get(key)
                        .copied()
                        .ok_or_else(|| DecompositionError::UnknownBlocker {
                            issue: issue.key.clone(),
                            blocker: key.clone(),
                        })?;
                local.push(position);
            }
        }
        blockers.push(local);
    }
    let order = creation_order(proposal, &blockers)?;
    let reach = reachability(&blockers);

    let issues: Vec<PreviewIssue> = order
        .iter()
        .filter_map(|&position| proposal.issues.get(position))
        .map(|issue| PreviewIssue {
            key: issue.key.clone(),
            phase: issue.phase.clone(),
            title: issue.title.clone(),
            body: render_body(issue, proposal.parent),
            owned_paths: issue.owned_paths.clone(),
            acceptance: issue.acceptance.clone(),
            blocked_by: issue.blocked_by.clone(),
        })
        .collect();

    let mut overlaps = Vec::new();
    for (left_order, &left) in order.iter().enumerate() {
        for &right in order.iter().skip(left_order + 1) {
            let (Some(a), Some(b)) = (proposal.issues.get(left), proposal.issues.get(right)) else {
                continue;
            };
            let paths: Vec<(OwnedPath, OwnedPath)> = a
                .owned_paths
                .iter()
                .flat_map(|x| b.owned_paths.iter().map(move |y| (x, y)))
                .filter(|(x, y)| x.overlaps(y))
                .map(|(x, y)| (x.clone(), y.clone()))
                .collect();
            if paths.is_empty() {
                continue;
            }
            let reaches = |from: usize, to: usize| {
                reach.get(from).is_some_and(|targets| targets.contains(&to))
            };
            // `reach[i]` holds what must land before `i`; creation order puts
            // `left` first, so only `right` can depend on it.
            let resolution = if reaches(right, left) {
                OverlapResolution::Ordered {
                    first: a.key.clone(),
                }
            } else {
                OverlapResolution::Unordered
            };
            overlaps.push(Overlap {
                left: a.key.clone(),
                right: b.key.clone(),
                paths,
                resolution,
            });
        }
    }

    let issues_count = u32::try_from(count).map_err(|_| DecompositionError::IssueCount)?;
    let dependencies: usize = proposal.issues.iter().map(|i| i.blocked_by.len()).sum();
    let writes = WriteCount {
        issues: issues_count,
        sub_issue_links: if proposal.parent.is_some() {
            issues_count
        } else {
            0
        },
        dependencies: u32::try_from(dependencies).map_err(|_| DecompositionError::IssueCount)?,
    };
    let digest = digest(&DigestInput {
        repository: &proposal.repository,
        parent: proposal.parent,
        issues: &issues,
    })?;
    Ok(Preview {
        repository: proposal.repository.clone(),
        parent: proposal.parent,
        issues,
        overlaps,
        writes,
        digest,
    })
}

fn text(value: &str, max: usize, multiline: bool) -> bool {
    !value.trim().is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|c| !c.is_control() || (multiline && c == '\n'))
}

fn validate_issue(issue: &ProposedIssue) -> Result<()> {
    if !text(&issue.title, MAX_TITLE_BYTES, false) || issue.title.trim() != issue.title {
        return Err(field("title"));
    }
    if issue
        .phase
        .as_ref()
        .is_some_and(|phase| !text(phase, MAX_PHASE_BYTES, false))
    {
        return Err(field("phase"));
    }
    if !text(&issue.outcome, MAX_OUTCOME_BYTES, true) {
        return Err(field("outcome"));
    }
    if issue.owned_paths.is_empty() || issue.owned_paths.len() > MAX_OWNED_PATHS {
        return Err(field("ownedPaths"));
    }
    if issue.acceptance.is_empty()
        || issue.acceptance.len() > MAX_CRITERIA
        || !issue
            .acceptance
            .iter()
            .all(|criterion| text(criterion, MAX_CRITERION_BYTES, false))
    {
        return Err(field("acceptance"));
    }
    if issue.blocked_by.len() > MAX_BLOCKERS {
        return Err(field("blockedBy"));
    }
    Ok(())
}

/// A topological order of the proposal that keeps the proposal's own order
/// wherever the dependencies allow it, or the cycle that prevents one.
fn creation_order(proposal: &Proposal, blockers: &[Vec<usize>]) -> Result<Vec<usize>> {
    let count = blockers.len();
    let mut placed = vec![false; count];
    let mut order = Vec::with_capacity(count);
    while order.len() < count {
        let next = (0..count).find(|&candidate| {
            !placed.get(candidate).copied().unwrap_or(true)
                && blockers.get(candidate).is_some_and(|before| {
                    before
                        .iter()
                        .all(|&blocker| placed.get(blocker).copied().unwrap_or(false))
                })
        });
        let Some(next) = next else {
            return Err(DecompositionError::Cycle(find_cycle(proposal, blockers, &placed)).into());
        };
        if let Some(slot) = placed.get_mut(next) {
            *slot = true;
        }
        order.push(next);
    }
    Ok(order)
}

/// A cycle among the unplaced issues. Every unplaced issue has an unplaced
/// blocker, so walking blockers from any of them must revisit one.
fn find_cycle(proposal: &Proposal, blockers: &[Vec<usize>], placed: &[bool]) -> Vec<IssueKey> {
    let unplaced = |position: usize| !placed.get(position).copied().unwrap_or(true);
    let key = |position: usize| proposal.issues.get(position).map(|issue| issue.key.clone());
    let Some(mut current) = (0..blockers.len()).find(|&position| unplaced(position)) else {
        return Vec::new();
    };
    let mut path: Vec<usize> = Vec::new();
    loop {
        if let Some(start) = path.iter().position(|&seen| seen == current) {
            let mut cycle: Vec<IssueKey> = path
                .get(start..)
                .unwrap_or_default()
                .iter()
                .filter_map(|&position| key(position))
                .collect();
            if let Some(first) = cycle.first().cloned() {
                cycle.push(first);
            }
            return cycle;
        }
        path.push(current);
        let next = blockers
            .get(current)
            .and_then(|before| before.iter().copied().find(|&blocker| unplaced(blocker)));
        match next {
            Some(next) => current = next,
            // Unreachable while every unplaced issue has an unplaced blocker;
            // return the walk rather than panic if that ever changes.
            None => return path.iter().filter_map(|&position| key(position)).collect(),
        }
    }
}

/// For each issue, every proposed issue that must land before it.
fn reachability(blockers: &[Vec<usize>]) -> Vec<BTreeSet<usize>> {
    (0..blockers.len())
        .map(|start| {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<usize> = blockers.get(start).cloned().unwrap_or_default();
            while let Some(position) = stack.pop() {
                if seen.insert(position) {
                    stack.extend(blockers.get(position).into_iter().flatten().copied());
                }
            }
            seen
        })
        .collect()
}

fn render_body(issue: &ProposedIssue, parent: Option<IssueNumber>) -> String {
    let mut body = String::new();
    if let Some(parent) = parent {
        let _ = writeln!(body, "Parent: #{}\n", parent.get());
    }
    let _ = writeln!(
        body,
        "## Outcome\n\n{}\n\n## Ownership\n",
        issue.outcome.trim()
    );
    for path in &issue.owned_paths {
        let _ = writeln!(body, "- `{}`", path.as_str());
    }
    let _ = writeln!(body, "\n## Acceptance criteria\n");
    for criterion in &issue.acceptance {
        let _ = writeln!(body, "- [ ] {}", criterion.trim());
    }
    let existing: Vec<String> = issue
        .blocked_by
        .iter()
        .filter_map(|blocker| match blocker {
            Blocker::Existing(number) => Some(format!("- #{}", number.get())),
            Blocker::Proposed(_) => None,
        })
        .collect();
    if !existing.is_empty() {
        let _ = write!(body, "\n## Dependencies\n\n{}\n", existing.join("\n"));
    }
    body
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DigestInput<'a> {
    repository: &'a Repository,
    parent: Option<IssueNumber>,
    issues: &'a [PreviewIssue],
}

fn digest(input: &DigestInput<'_>) -> Result<PreviewDigest> {
    let bytes = serde_json::to_vec(input).map_err(|_| DecompositionError::Encoding)?;
    let mut hasher = Sha256::new();
    hasher.update(b"kitchen-decomposition-preview-v1\0");
    hasher.update(&bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out.parse()
}

/// A person's approval of one exact preview.
///
/// Build it only from what a person present agreed to, with the digest of
/// the preview they were shown. It is not persisted as a standing grant: each
/// [`apply`] call needs it again, and it covers only the writes of that
/// preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// A reference for this approval, chosen by the session.
    pub id: ExternalRef,
    /// The person who approved.
    pub given_by: HolderId,
    /// The digest of the preview they approved.
    pub digest: PreviewDigest,
}

/// The durable store, forge executor, and house grants [`apply`] writes
/// through.
pub struct Writer<'a, T> {
    /// The house's durable store.
    pub store: &'a HouseStore,
    /// The house-scoped GitHub executor.
    pub executor: &'a GitHubExecutor<T>,
    /// The house's current grants; they must permit issue creation and
    /// relationship edits on the repository.
    pub grants: &'a HouseGrants,
    /// Time source.
    pub clock: &'a dyn Clock,
}

/// Bounds for the decomposition task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Instruction revisions pinned on the task.
    pub provenance: Provenance,
    /// Lease on the task while this call writes.
    pub lease: LeaseTtl,
}

/// What kind of write one step is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WriteKind {
    /// Create the issue.
    Create,
    /// Link the issue as a sub-issue of the parent.
    Parent,
    /// Record a blocked-by link.
    BlockedBy {
        /// The blocking issue.
        blocker: Blocker,
    },
}

/// One write that was applied, in this call or an earlier one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Written {
    /// The issue the write concerns.
    pub issue: IssueKey,
    /// The write.
    pub write: WriteKind,
    /// The forge reference from the receipt.
    pub reference: ExternalRef,
    /// Whether this call submitted it, or it was already applied.
    pub reused: bool,
}

/// How an [`apply`] call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ApplyOutcome {
    /// Every write is applied and the task settled.
    Completed,
    /// The approval names another digest: the proposal changed since the
    /// person looked, or they approved something else. Nothing was written.
    StaleApproval,
    /// Unordered ownership overlaps remain. Nothing was written.
    NotReady,
    /// The preview needs more writes than the house's posting budget allows
    /// for one task. Nothing was written.
    OverBudget {
        /// Writes the preview needs.
        needed: u32,
        /// The house's per-task limit.
        limit: u32,
    },
    /// An earlier decomposition of this repository is unfinished and may
    /// have written to the forge. Finish or cancel it first. Nothing was
    /// written.
    EarlierUnfinished {
        /// The unfinished task.
        task: TaskId,
    },
    /// Another run holds this decomposition's task.
    HeldElsewhere,
    /// A write's outcome is unknown; nothing after it was submitted. The next
    /// run reconciles it first.
    Uncertain {
        /// The write's logical name.
        effect: EffectName,
    },
    /// The forge refused a write. A later run retries within the task's
    /// attempt budget.
    NotApplied {
        /// The write's logical name.
        effect: EffectName,
        /// Why.
        reason: NotAppliedReason,
    },
    /// The task had settled before this call.
    Settled(Settlement),
}

/// The result of [`apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// The preview this call acted on.
    pub preview: Preview,
    /// The decomposition task, once one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// Created issue numbers by key, as far as known.
    pub issues: BTreeMap<IssueKey, IssueNumber>,
    /// Applied writes, in write order.
    pub written: Vec<Written>,
    /// How the call ended.
    pub outcome: ApplyOutcome,
}

/// The task id of the decomposition bound to `digest`.
///
/// # Errors
/// Never for a valid digest; the id is a fixed prefix and 32 hex digits.
pub fn task_id(digest: &PreviewDigest) -> Result<TaskId> {
    let hex = digest.hex().get(..32).ok_or(DecompositionError::Encoding)?;
    Ok(TaskId::new(&format!("{TASK_PREFIX}{hex}"))?)
}

struct Step {
    issue: IssueKey,
    write: WriteKind,
    name: EffectName,
}

fn steps(preview: &Preview) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for issue in &preview.issues {
        steps.push(Step {
            issue: issue.key.clone(),
            write: WriteKind::Create,
            name: EffectName::new(&format!("issue-{}", issue.key))?,
        });
    }
    if preview.parent.is_some() {
        for issue in &preview.issues {
            steps.push(Step {
                issue: issue.key.clone(),
                write: WriteKind::Parent,
                name: EffectName::new(&format!("parent-{}", issue.key))?,
            });
        }
    }
    for issue in &preview.issues {
        for blocker in &issue.blocked_by {
            let target = match blocker {
                Blocker::Proposed(key) => key.to_string(),
                Blocker::Existing(number) => format!("n{}", number.get()),
            };
            steps.push(Step {
                issue: issue.key.clone(),
                write: WriteKind::BlockedBy {
                    blocker: blocker.clone(),
                },
                name: EffectName::new(&format!("blocked-{}-by-{target}", issue.key))?,
            });
        }
    }
    Ok(steps)
}

/// Write the approved preview to the forge, or complete an earlier partial
/// write of it.
///
/// Recomputes the preview from `proposal` and writes nothing unless
/// `approval` names its digest, it is ready, and its writes fit the house's
/// posting budget. Then, as one durable task, it reconciles earlier uncertain
/// writes, reuses every write already applied, and submits the rest in
/// order. See the module documentation for the guarantees.
///
/// # Errors
/// [`DecompositionError::ApprovalNeedsPerson`] for a non-interactive
/// claimant; [`preview`]'s errors; [`DecompositionError::UnreadableReceipt`]
/// when a created issue's receipt names no issue; executor, contract, and
/// store errors, which leave interrupted work for the next run.
pub fn apply<T: GitHubMutationTransport>(
    writer: &Writer<'_, T>,
    proposal: &Proposal,
    approval: &Approval,
    claimant: &Claimant,
    options: &ApplyOptions,
) -> Result<ApplyReport> {
    match claimant.trigger {
        Trigger::Interactive => {}
        Trigger::Scheduled | Trigger::Event(_) => {
            return Err(DecompositionError::ApprovalNeedsPerson.into());
        }
    }
    let preview = preview(proposal)?;
    let report = |preview: Preview, task: Option<TaskId>, outcome| ApplyReport {
        preview,
        task,
        issues: BTreeMap::new(),
        written: Vec::new(),
        outcome,
    };
    if approval.digest != preview.digest {
        return Ok(report(preview, None, ApplyOutcome::StaleApproval));
    }
    if !preview.ready() {
        return Ok(report(preview, None, ApplyOutcome::NotReady));
    }
    let steps = steps(&preview)?;
    // Building an effect only validates it: this checks the house permits
    // issue creation here and reads its per-task posting limit, before any
    // task exists.
    let probe = writer.executor.effect(GitHubMutation {
        repository: preview.repository.clone(),
        action: create_action(&preview, 0)?,
    })?;
    let needed = preview.writes.total();
    let limit = probe.posting_budget.limit();
    if needed > limit {
        return Ok(report(
            preview,
            None,
            ApplyOutcome::OverBudget { needed, limit },
        ));
    }
    let id = task_id(&preview.digest)?;
    let store = writer.store;
    let spec = TaskSpec {
        id: id.clone(),
        role: Role::SousChef,
        repository: Some(preview.repository.clone()),
        authority: TaskAuthority::delegate(writer.grants, [])?,
        retry: RetryPolicy::new(ATTEMPTS, BUDGET)?,
        provenance: options.provenance.clone(),
        resources: BTreeSet::new(),
        requires: CapabilityRequirements::new(),
        agent: None,
    };
    // The repository's decomposition slot is the task itself: the check
    // for an earlier unfinished task and the creation and claim of this one
    // happen in one store transaction, so two different approved previews
    // cannot both pass the check.
    let now = writer.clock.now();
    let reservation = store.reserve_task(spec, claimant, options.lease, now, |tasks| {
        Ok(earlier_unfinished(tasks, &id, &preview.repository, now))
    })?;
    let fence = match reservation {
        Reservation::Reserved(lease) => lease.fence(),
        Reservation::Blocked(task) => {
            return Ok(report(
                preview,
                None,
                ApplyOutcome::EarlierUnfinished { task },
            ));
        }
        Reservation::Existing => {
            let record = store.task(&id)?;
            let now = writer.clock.now();
            let lease = match record.state() {
                TaskState::Settled { settlement, .. } => {
                    let mut done = report(preview, Some(id), ApplyOutcome::Settled(*settlement));
                    collect_applied(&record, &steps, &mut done)?;
                    return Ok(done);
                }
                TaskState::Open => store.claim(&id, claimant, options.lease, now),
                TaskState::Claimed { lease } if lease.is_live(now) => {
                    return Ok(report(preview, Some(id), ApplyOutcome::HeldElsewhere));
                }
                TaskState::Claimed { .. } => store.take_over(&id, claimant, options.lease, now),
            };
            match lease {
                Ok(lease) => lease.fence(),
                Err(Error::State(
                    StateError::ClaimHeld { .. }
                    | StateError::LeaseExpired { .. }
                    | StateError::LeaseLive { .. },
                )) => return Ok(report(preview, Some(id), ApplyOutcome::HeldElsewhere)),
                Err(error) => return Err(error),
            }
        }
    };
    let reconciled = reconcile(store, writer.executor, &id, fence, writer.clock)?;
    if let Some(stuck) = reconciled
        .unresolved
        .first()
        .or_else(|| reconciled.foreign.first())
    {
        let effect = stuck.name().clone();
        store.relinquish(&id, fence, writer.clock.now())?;
        return Ok(report(
            preview,
            Some(id),
            ApplyOutcome::Uncertain { effect },
        ));
    }
    let attempt = match store.start_attempt(&id, fence, writer.clock.now())? {
        AttemptStart::Started(attempt) | AttemptStart::AlreadyRunning(attempt) => attempt,
        AttemptStart::Exhausted => {
            let mut done = report(
                preview,
                Some(id.clone()),
                ApplyOutcome::Settled(Settlement::Exhausted),
            );
            collect_applied(&store.task(&id)?, &steps, &mut done)?;
            return Ok(done);
        }
    };
    let mut run = report(preview, Some(id.clone()), ApplyOutcome::Completed);
    collect_applied(&store.task(&id)?, &steps, &mut run)?;
    let mut context = Attempt {
        writer,
        approval,
        id: &id,
        fence,
        attempt,
    };
    for step in &steps {
        if run
            .written
            .iter()
            .any(|written| written.issue == step.issue && written.write == step.write)
        {
            continue;
        }
        // A write whose earlier outcome is still unknown, even one a person
        // waived, is never submitted again: it may have created an issue.
        let unknown = store
            .task(&id)?
            .effects()
            .iter()
            .any(|effect| effect.name() == &step.name && !effect.state().is_resolved());
        if unknown {
            store.relinquish(&id, fence, writer.clock.now())?;
            run.outcome = ApplyOutcome::Uncertain {
                effect: step.name.clone(),
            };
            return Ok(run);
        }
        let action = action(&run, step)?;
        let effect = writer.executor.effect(GitHubMutation {
            repository: run.preview.repository.clone(),
            action,
        })?;
        let record = context.run(step, effect.into())?;
        match record.state() {
            EffectState::Applied { receipt, .. } => {
                if step.write == WriteKind::Create {
                    let number = issue_number(&run.preview.repository, receipt.reference())?;
                    run.issues.insert(step.issue.clone(), number);
                }
                run.written.push(Written {
                    issue: step.issue.clone(),
                    write: step.write.clone(),
                    reference: receipt.reference().clone(),
                    reused: false,
                });
            }
            EffectState::NotApplied { reason, .. } => {
                let reason = *reason;
                store.finish_attempt(
                    &id,
                    fence,
                    attempt,
                    AttemptOutcome::Failed(FailureClass::Retryable),
                    writer.clock.now(),
                )?;
                release(store, &id, fence, writer.clock)?;
                run.outcome = ApplyOutcome::NotApplied {
                    effect: step.name.clone(),
                    reason,
                };
                return Ok(run);
            }
            EffectState::Intended
            | EffectState::Uncertain { .. }
            | EffectState::Unresolvable { .. }
            | EffectState::Waived { .. } => {
                store.relinquish(&id, fence, writer.clock.now())?;
                run.outcome = ApplyOutcome::Uncertain {
                    effect: step.name.clone(),
                };
                return Ok(run);
            }
        }
    }
    store.finish_attempt(
        &id,
        fence,
        attempt,
        AttemptOutcome::Succeeded,
        writer.clock.now(),
    )?;
    Ok(run)
}

/// Give the claim back after a retryable failure unless the task settled.
fn release(
    store: &HouseStore,
    id: &TaskId,
    fence: crate::contracts::Fence,
    clock: &dyn Clock,
) -> Result<()> {
    if matches!(store.task(id)?.state(), TaskState::Claimed { .. }) {
        store.relinquish(id, fence, clock.now())?;
    }
    Ok(())
}

struct Attempt<'a, 'w, T> {
    writer: &'a Writer<'w, T>,
    approval: &'a Approval,
    id: &'a TaskId,
    fence: crate::contracts::Fence,
    attempt: AttemptNumber,
}

impl<T: GitHubMutationTransport> Attempt<'_, '_, T> {
    fn run(&mut self, step: &Step, effect: Effect) -> Result<EffectRecord> {
        let store = self.writer.store;
        let revision = store.task(self.id)?.evidence().revision();
        let consent = Consent {
            id: consent_id(self.approval, self.attempt, &step.name)?,
            given_by: self.approval.given_by.clone(),
            house: store.house().clone(),
            task: self.id.clone(),
            effect: effect.clone(),
            revision,
        };
        let plan = EffectPlan {
            task: self.id.clone(),
            fence: self.fence,
            name: step.name.clone(),
            decided_at: revision,
            effect,
            consent: Some(consent),
        };
        run_effect(
            store,
            self.writer.executor,
            self.writer.grants,
            plan,
            self.writer.clock,
        )
    }
}

/// A consent reference unique to the approval, attempt, and write, since the
/// store refuses one consent for two effects.
fn consent_id(
    approval: &Approval,
    attempt: AttemptNumber,
    name: &EffectName,
) -> Result<ExternalRef> {
    let mut hasher = Sha256::new();
    hasher.update(b"kitchen-decomposition-consent-v1\0");
    hasher.update(approval.id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(approval.digest.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(attempt.get().to_be_bytes());
    hasher.update(name.as_str().as_bytes());
    let mut out = String::from("decomposition-consent-");
    for byte in hasher.finalize().iter().take(16) {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(ExternalRef::new(&out)?)
}

/// Fill `report` with every write already applied in any attempt, and the
/// issue numbers they created.
fn collect_applied(record: &TaskRecord, steps: &[Step], report: &mut ApplyReport) -> Result<()> {
    for step in steps {
        let applied = record.effects().iter().rev().find_map(|effect| {
            match (effect.name() == &step.name, effect.state()) {
                (true, EffectState::Applied { receipt, .. }) => Some(receipt),
                _ => None,
            }
        });
        let Some(receipt) = applied else {
            continue;
        };
        if step.write == WriteKind::Create {
            let number = issue_number(&report.preview.repository, receipt.reference())?;
            report.issues.insert(step.issue.clone(), number);
        }
        report.written.push(Written {
            issue: step.issue.clone(),
            write: step.write.clone(),
            reference: receipt.reference().clone(),
            reused: true,
        });
    }
    Ok(())
}

/// The unfinished decomposition of `repository` other than `own`, if any: a
/// task that has not settled and either holds a live claim or has a write
/// that may have reached the forge. A task that only ever recorded refused
/// or unsent writes, and is not being run, wrote nothing and frees the slot.
fn earlier_unfinished(
    tasks: &[&TaskRecord],
    own: &TaskId,
    repository: &Repository,
    now: Timestamp,
) -> Option<TaskId> {
    tasks
        .iter()
        .find(|task| {
            task.spec().id != *own
                && task.spec().id.as_str().starts_with(TASK_PREFIX)
                && task.spec().repository.as_ref() == Some(repository)
                && match task.state() {
                    TaskState::Settled { .. } => false,
                    TaskState::Claimed { lease } if lease.is_live(now) => true,
                    TaskState::Open | TaskState::Claimed { .. } => task
                        .effects()
                        .iter()
                        .any(|effect| !matches!(effect.state(), EffectState::NotApplied { .. })),
                }
        })
        .map(|task| task.spec().id.clone())
}

fn create_action(preview: &Preview, position: usize) -> Result<GitHubAction> {
    let issue = preview
        .issues
        .get(position)
        .ok_or(DecompositionError::IssueCount)?;
    Ok(GitHubAction::CreateIssue {
        title: Text::new(&issue.title)?,
        body: Text::new(&issue.body)?,
    })
}

fn action(run: &ApplyReport, step: &Step) -> Result<GitHubAction> {
    let created = |key: &IssueKey| {
        run.issues
            .get(key)
            .copied()
            .ok_or(DecompositionError::UnreadableReceipt)
    };
    match &step.write {
        WriteKind::Create => {
            let position = run
                .preview
                .position(&step.issue)
                .ok_or(DecompositionError::IssueCount)?;
            create_action(&run.preview, position)
        }
        WriteKind::Parent => Ok(GitHubAction::LinkSubIssue {
            parent: run.preview.parent.ok_or(DecompositionError::Encoding)?,
            child: created(&step.issue)?,
        }),
        WriteKind::BlockedBy { blocker } => Ok(GitHubAction::LinkDependency {
            issue: created(&step.issue)?,
            blocker: match blocker {
                Blocker::Proposed(key) => created(key)?,
                Blocker::Existing(number) => *number,
            },
        }),
    }
}

/// The issue number in a receipt reference of the form
/// `https://github.com/<repository>/issues/<number>`.
fn issue_number(repository: &Repository, reference: &ExternalRef) -> Result<IssueNumber> {
    let prefix = format!("https://github.com/{}/issues/", repository.as_str());
    reference
        .as_str()
        .strip_prefix(&prefix)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse::<u64>().ok())
        .and_then(|number| IssueNumber::new(number).ok())
        .ok_or_else(|| DecompositionError::UnreadableReceipt.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn owned_paths_overlap_only_at_segment_boundaries() -> TestResult {
        let dir = OwnedPath::new("crates/kitchen/src/workflows/")?;
        let file = OwnedPath::new("crates/kitchen/src/workflows/gate.rs")?;
        let sibling = OwnedPath::new("crates/kitchen/src/workflows-extra")?;
        assert_eq!(dir.as_str(), "crates/kitchen/src/workflows");
        assert!(dir.overlaps(&file) && file.overlaps(&dir));
        assert!(dir.overlaps(&dir));
        assert!(!dir.overlaps(&sibling));
        for bad in ["", "/", "/etc", "a/../b", "a//b", "./a", "a\\b", "a\tb"] {
            assert!(OwnedPath::new(bad).is_err(), "{bad:?} must be refused");
        }
        Ok(())
    }

    #[test]
    fn issue_keys_are_bounded_lowercase_slugs() -> TestResult {
        assert_eq!(IssueKey::new("core-1")?.as_str(), "core-1");
        let longest = "a".repeat(MAX_KEY_BYTES);
        assert!(IssueKey::new(&longest).is_ok());
        for bad in [
            "",
            "-a",
            "Core",
            "a_b",
            "a b",
            &"a".repeat(MAX_KEY_BYTES + 1),
        ] {
            assert!(IssueKey::new(bad).is_err(), "{bad:?} must be refused");
        }
        Ok(())
    }

    #[test]
    fn receipts_must_name_an_issue_in_the_repository() -> TestResult {
        let repo = Repository::new("sample/project")?;
        let good = ExternalRef::new("https://github.com/sample/project/issues/42")?;
        assert_eq!(issue_number(&repo, &good)?.get(), 42);
        for bad in [
            "https://github.com/other/project/issues/42",
            "https://github.com/sample/project/pull/42",
            "https://github.com/sample/project/issues/0",
            "https://github.com/sample/project/issues/42#x",
            "kitchen-key",
        ] {
            assert!(issue_number(&repo, &ExternalRef::new(bad)?).is_err());
        }
        Ok(())
    }

    #[test]
    fn digests_parse_only_in_canonical_form() {
        let hex = "0".repeat(64);
        assert!(format!("sha256:{hex}").parse::<PreviewDigest>().is_ok());
        assert!(hex.parse::<PreviewDigest>().is_err());
        assert!(
            format!("sha256:{}", "A".repeat(64))
                .parse::<PreviewDigest>()
                .is_err()
        );
        assert!("sha256:abc".parse::<PreviewDigest>().is_err());
    }
}
