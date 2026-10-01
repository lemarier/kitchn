//! The scheduled pickup pass: claim ready issues of one repository and
//! launch their workers, and launch the next attempt of a scheduled task
//! whose earlier attempt ended.

use std::fmt;

use super::{
    Outcome, Pass, RunError, TASK_LEASE, awaiting_launch, held_by_run, issue_of, repair_of,
    run_claimant, transfer, writer_open, writing,
};
use crate::{
    ConsumerId, TaskId,
    contracts::{
        AttemptNumber, BranchName, Clock, ContractError, Fence, LeaseTtl, Repository, ResourceRef,
        Text, WorkerBackend, Workspace,
    },
    house::HouseConfig,
    integrations::github::{GitHubClient, GitHubReadTransport, Issue, IssueDetail, IssueState},
    state::{HouseStore, TaskRecord, TaskState},
    workflows::{
        coordination::{BranchFact, Context, LaunchOutcome, MailboxRoute, Standing, task_branch},
        known,
        pickup::{
            Base, Blocker, Blockers, Candidate, ClaimOutcome, IssueRef, LinkedWork, Overlap,
            PickupPolicy, PinnedInstructions, Readiness, TaskTemplate, WorkerBrief, claim_issue,
            select, work_branch,
        },
        tick::PassRun,
    },
};

type Result<T> = std::result::Result<T, crate::Error>;

/// Ready issues one pass inspects, lowest number first. Each costs an issue,
/// a dependency, and a timeline read.
pub const MAX_READY_INSPECTED: usize = 20;

/// Acceptance criteria taken from one issue.
const MAX_ACCEPTANCE: usize = 16;

/// The house's pickup labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupLabels {
    /// Marks an issue ready for an agent.
    pub ready: String,
    /// Marks an issue that needs a specification pass first.
    pub needs_spec: String,
    /// Reserves an issue for a person.
    pub human_only: String,
}

/// What a pickup pass works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupSettings {
    /// The one repository this pass serves.
    pub repository: Repository,
    /// Label names.
    pub labels: PickupLabels,
    /// Most unsettled scheduled pickup tasks in the repository. Without
    /// observed file overlap a pass still launches at most one writer, so a
    /// capacity above one does not add concurrent writers.
    pub capacity: u32,
    /// Workers create `<prefix>/issue-<number>`.
    pub branch_prefix: BranchName,
    /// The pinned instructions every new task records and every brief names.
    pub instructions: PinnedInstructions,
    /// Where a worker writes its evidence report inside its workspace.
    pub report_path: Text,
}

/// One scheduled pickup pass.
pub struct PickupPass<'a, T> {
    /// The house store.
    pub store: &'a HouseStore,
    /// The house configuration.
    pub house: &'a HouseConfig,
    /// The house's worker backend.
    pub backend: &'a dyn WorkerBackend,
    /// The house's forge reads.
    pub forge: &'a GitHubClient<T>,
    /// Time source.
    pub clock: &'a dyn Clock,
    /// What to pick up.
    pub settings: &'a PickupSettings,
    /// Take over an expired pass lease instead of stopping.
    pub take_over: bool,
    /// The house tick's run this pass serves, if a tick started it: each
    /// task is recorded on it before its claim, and it is renewed with the
    /// pass lease.
    pub tick: Option<&'a PassRun>,
}

/// What a pickup pass did about one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickupAction {
    /// The issue was claimed and its worker launch was accepted.
    Launched {
        /// The issue.
        issue: IssueRef,
        /// Its task.
        task: TaskId,
        /// The attempt launched.
        attempt: AttemptNumber,
        /// The worker the backend created, its own reference.
        worker: ResourceRef,
    },
    /// Claimed, or held from an earlier pass, but not launched now.
    NotLaunched {
        /// The issue.
        issue: IssueRef,
        /// Its task.
        task: TaskId,
        /// Why.
        outcome: LaunchOutcome,
    },
    /// Selected, but the claim went elsewhere.
    NotClaimed {
        /// The issue.
        issue: IssueRef,
        /// What the claim found.
        outcome: ClaimOutcome,
    },
    /// A scheduled task's issue is no longer open; its next attempt waits
    /// for a person.
    IssueClosed {
        /// The task.
        task: TaskId,
    },
    /// A scheduled task's earlier attempt was a stacked layer; its next
    /// attempt needs the lower layer, which this pass does not know.
    StackedRetry {
        /// The task.
        task: TaskId,
    },
    /// A scheduled task changed hands before this pass could take it for
    /// its next attempt; it was left alone.
    Moved {
        /// The task.
        task: TaskId,
    },
    /// An expired task claim needs coordination before pickup can retry it.
    TaskClaimUncertain {
        /// The task.
        task: TaskId,
    },
}

impl fmt::Display for PickupAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launched {
                issue,
                task,
                attempt,
                ..
            } => write!(
                formatter,
                "launched {issue} task {task} attempt {}",
                attempt.get()
            ),
            Self::NotLaunched {
                issue,
                task,
                outcome,
            } => write!(formatter, "not launched {issue} task {task}: {outcome:?}"),
            Self::NotClaimed { issue, outcome } => {
                write!(formatter, "not claimed {issue}: {outcome:?}")
            }
            Self::IssueClosed { task } => {
                write!(formatter, "issue closed, next attempt waits: task {task}")
            }
            Self::StackedRetry { task } => write!(
                formatter,
                "stacked task needs a person for its next attempt: task {task}"
            ),
            Self::TaskClaimUncertain { task } => write!(
                formatter,
                "task {task} has an expired task claim; --take-over on pickup covers its pass lease, not this task: inspect the launch, then run kitchn run coordinate --take-over"
            ),
            Self::Moved { task } => write!(formatter, "task {task} changed hands; left alone"),
        }
    }
}

impl<T: GitHubReadTransport> PickupPass<'_, T> {
    /// Run one pass under the repository's pickup lease.
    ///
    /// # Errors
    /// Refuses, before taking the lease, a repository outside the house,
    /// instructions or a backend of another house, and a backend that does
    /// not support supervision on its mailbox route. Returns forge read
    /// failures, which stop the pass, and store and authority failures.
    pub fn run(&self) -> Result<Outcome<PickupAction>> {
        let settings = self.settings;
        if !self.house.repositories.contains(&settings.repository) {
            return Err(RunError::RepositoryOutsideHouse.into());
        }
        let descriptor = self.backend.descriptor();
        for found in [&descriptor.house, &settings.instructions.house] {
            if found != self.store.house() {
                return Err(ContractError::CrossHouse {
                    expected: self.store.house().clone(),
                    found: found.clone(),
                }
                .into());
            }
        }
        let route = MailboxRoute::select(descriptor);
        descriptor
            .capabilities
            .require(route.worker_requirements().iter().copied())?;
        let template =
            super::task_template(self.house, route, settings.instructions.provenance.clone())?;
        let consumer = Pass::Pickup.consumer(&settings.repository)?;
        super::under_lease(self.store, &consumer, self.take_over, self.clock, |fence| {
            self.pass(&template, &consumer, fence)
        })
    }

    fn pass(
        &self,
        template: &TaskTemplate,
        consumer: &ConsumerId,
        lease: Fence,
    ) -> Result<Vec<PickupAction>> {
        let settings = self.settings;
        let (house, repository) = (self.store.house(), &settings.repository);
        let now = self.clock.now();
        let issues =
            known(
                self.forge
                    .issues_filtered(house, repository, Some(IssueState::Open), None),
            )?;
        let tasks = self.store.tasks()?;
        let uncertain: Vec<_> = tasks
            .iter()
            .filter_map(|record| {
                issue_of(record, repository)?;
                if awaiting_launch(record)
                    && matches!(record.state(), TaskState::Claimed { lease }
                    if lease.holder().as_str() == super::RUN_HOLDER && !lease.is_live(now))
                {
                    Some(PickupAction::TaskClaimUncertain {
                        task: record.spec().id.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
        if !uncertain.is_empty() {
            return Ok(uncertain);
        }
        // One writer per repository: file overlap is not observed, so a pass
        // launches at most one writer, and none while another branch writer
        // of the repository, scheduled or a person's, may be working.
        // Selection already leaves new issues while any pickup task is
        // unsettled or a repair round may be working.
        let retries: Vec<(&TaskRecord, IssueRef, Fence)> = if writer_open(&tasks, repository) {
            Vec::new()
        } else {
            tasks
                .iter()
                .filter_map(|record| {
                    let issue = issue_of(record, repository)?;
                    let fence = held_by_run(record, now)?.fence();
                    awaiting_launch(record).then_some((record, issue, fence))
                })
                .collect()
        };
        let mut ready: Vec<&Issue> = issues
            .iter()
            .filter(|issue| has_label(issue, &settings.labels.ready))
            .collect();
        ready.sort_by_key(|issue| issue.number.get());
        let mut details = Vec::new();
        let mut candidates = Vec::new();
        for issue in ready.into_iter().take(MAX_READY_INSPECTED) {
            let detail = known(self.forge.issue_detail(house, repository, issue.number))?;
            candidates.push(self.candidate(issue, &detail, &tasks)?);
            details.push(detail);
        }
        let policy = PickupPolicy {
            repositories: vec![repository.clone()],
            capacity: settings.capacity,
        };
        let selection = select(&policy, &candidates, &tasks, now)?;
        if selection.picks.is_empty() && retries.is_empty() {
            return Ok(Vec::new());
        }
        let grants = super::standing_grants(self.house)?;
        let ctx = Context {
            store: self.store,
            backend: self.backend,
            grants: &grants,
            clock: self.clock,
            consent: &Standing,
        };
        // Claims and launches are bound to this pass's lease: once it is
        // superseded, the store refuses them, even from a process already
        // past its last renewal.
        let claimant = run_claimant()?.under(consumer.clone(), lease);
        let ttl = LeaseTtl::new(TASK_LEASE)?;
        let mut actions = Vec::new();
        // Selection picks only while no scheduled task is unsettled, so the
        // first pick is the one writer.
        if let Some(pick) = selection.picks.into_iter().next() {
            super::renew(self.store, consumer, lease, self.tick, self.clock)?;
            super::record(self.store, self.tick, &pick.task, self.clock)?;
            match claim_issue(self.store, template, &pick.issue, &claimant, ttl, now)? {
                ClaimOutcome::Claimed(claim) | ClaimOutcome::Adopted(claim) => {
                    let body = details
                        .iter()
                        .find(|detail| detail.number == pick.issue.number)
                        .and_then(|detail| detail.body.as_deref());
                    let brief = self.brief(pick.issue.clone(), pick.base, body, 1)?;
                    actions.push(launch(&ctx, pick.task, claim.fence(), &brief)?);
                }
                outcome => actions.push(PickupAction::NotClaimed {
                    issue: pick.issue,
                    outcome,
                }),
            }
            return Ok(actions);
        }
        for (record, issue, fence) in retries {
            super::renew(self.store, consumer, lease, self.tick, self.clock)?;
            let task = record.spec().id.clone();
            if !issues.iter().any(|open| open.number == issue.number) {
                actions.push(PickupAction::IssueClosed { task });
                continue;
            }
            if task_branch(record).is_some_and(|branch| BranchFact::Stacked.holds(record, &branch))
            {
                actions.push(PickupAction::StackedRetry { task });
                continue;
            }
            let detail = known(self.forge.issue_detail(house, repository, issue.number))?;
            let next_attempt = record
                .attempts()
                .last()
                .map_or(1, |attempt| attempt.number().get() + 1);
            let brief = self.brief(
                issue,
                Base::DefaultBranch,
                detail.body.as_deref(),
                next_attempt,
            )?;
            // The task moves to this pass's claim before the launch, so the
            // claim it had cannot also act on it.
            super::record(self.store, self.tick, &task, self.clock)?;
            let Some(fence) = transfer(self.store, &task, fence, &claimant, now)? else {
                actions.push(PickupAction::Moved { task });
                continue;
            };
            actions.push(launch(&ctx, task, fence, &brief)?);
            break;
        }
        Ok(actions)
    }

    /// The pickup facts of one ready issue. File overlap is not observed:
    /// while another scheduled task of the repository is unsettled, overlap
    /// is unknown and the issue waits.
    fn candidate(
        &self,
        issue: &Issue,
        detail: &IssueDetail,
        tasks: &[TaskRecord],
    ) -> Result<Candidate> {
        let (house, repository) = (self.store.house(), &self.settings.repository);
        let labels = &self.settings.labels;
        let reference = IssueRef {
            repository: repository.clone(),
            number: issue.number,
        };
        let blockers = known(self.forge.dependencies(house, repository, issue.number))?
            .into_iter()
            .map(|blocker| Blocker {
                issue: IssueRef {
                    repository: blocker.repository,
                    number: blocker.number,
                },
                open: blocker.state != IssueState::Closed,
            })
            .collect();
        let linked = known(
            self.forge
                .linked_pull_requests(house, repository, issue.number),
        )?
        .into_iter()
        .find(|linked| linked.pull_request.state != IssueState::Closed)
        .map_or(LinkedWork::None, |linked| {
            LinkedWork::PullRequest(linked.pull_request.number)
        });
        let busy = tasks.iter().any(|record| {
            issue_of(record, repository).is_some_and(|other| other.number != issue.number)
                && !matches!(record.state(), TaskState::Settled { .. })
        }) || repairing(tasks, repository);
        let readiness = if has_label(issue, &labels.needs_spec)
            || acceptance(detail.body.as_deref()).is_empty()
        {
            Readiness::NeedsSpec
        } else {
            Readiness::Ready
        };
        Ok(Candidate {
            issue: reference,
            readiness,
            human_only: has_label(issue, &labels.human_only),
            assigned: !issue.assignees.is_empty(),
            blockers: Blockers::Known(blockers),
            prose_dependencies: Vec::new(),
            linked,
            overlap: if busy {
                Overlap::Unknown
            } else {
                Overlap::None
            },
            blocks_open: 0,
            milestone_due: None,
            created_at: detail.created_at,
        })
    }

    fn brief(
        &self,
        issue: IssueRef,
        base: Base,
        body: Option<&str>,
        attempt: u32,
    ) -> Result<WorkerBrief> {
        let settings = self.settings;
        let suffix = if attempt == 1 {
            String::new()
        } else {
            format!("-attempt-{attempt}")
        };
        Ok(WorkerBrief {
            branch: work_branch(&format!(
                "{}/issue-{}{}",
                settings.branch_prefix,
                issue.number.get(),
                suffix
            ))?,
            issue,
            base,
            instructions: settings.instructions.clone(),
            acceptance: acceptance(body),
            budget: self.house.follow_up_budget(),
            report_path: settings.report_path.clone(),
        })
    }
}

fn launch(
    ctx: &Context<'_>,
    task: TaskId,
    fence: Fence,
    brief: &WorkerBrief,
) -> Result<PickupAction> {
    let issue = brief.issue.clone();
    Ok(
        match crate::workflows::coordination::launch_worker(
            ctx,
            &task,
            fence,
            Workspace::Isolated,
            brief,
        )? {
            LaunchOutcome::Accepted { attempt, worker } => PickupAction::Launched {
                issue,
                task,
                attempt,
                worker,
            },
            outcome => PickupAction::NotLaunched {
                issue,
                task,
                outcome,
            },
        },
    )
}

/// Whether a repair or follow-up round of `repository`, scheduled or a
/// person's, may be working: its writer counts against the repository's one
/// writer.
fn repairing(tasks: &[TaskRecord], repository: &Repository) -> bool {
    tasks
        .iter()
        .any(|record| repair_of(record, repository).is_some() && writing(record))
}

fn has_label(issue: &Issue, name: &str) -> bool {
    issue
        .labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case(name))
}

/// The list items under the issue's first heading or line that starts with
/// "Acceptance", verbatim, at most [`MAX_ACCEPTANCE`]; blank lines between
/// items are allowed. Checkbox markers are
/// kept as written. Empty when the issue states none.
fn acceptance(body: Option<&str>) -> Vec<Text> {
    let Some(body) = body else {
        return Vec::new();
    };
    let mut lines = body.lines().skip_while(|line| {
        !line
            .trim_start_matches(['#', ' '])
            .to_ascii_lowercase()
            .starts_with("acceptance")
    });
    if lines.next().is_none() {
        return Vec::new();
    }
    lines
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| line.is_empty() || line.starts_with("- ") || line.starts_with("* "))
        .filter(|line| !line.is_empty())
        .filter_map(|line| Text::new(line.get(2..).unwrap_or_default().trim()).ok())
        .take(MAX_ACCEPTANCE)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::acceptance;

    fn items(body: &str) -> Vec<String> {
        acceptance(Some(body))
            .iter()
            .map(|text| text.as_str().to_owned())
            .collect()
    }

    #[test]
    fn acceptance_reads_the_list_under_its_heading() {
        let body = "Intro.\n\n## Acceptance criteria\n\n- [ ] The driver builds.\n\n* Tests pass.\n\nMore prose.\n- not acceptance";
        assert_eq!(items(body), ["[ ] The driver builds.", "Tests pass."]);
    }

    #[test]
    fn acceptance_without_a_heading_or_items_is_empty() {
        assert!(items("Just prose.\n- a list item").is_empty());
        assert!(items("Acceptance:\n\nnone listed").is_empty());
        assert!(acceptance(None).is_empty());
    }

    #[test]
    fn acceptance_is_bounded() {
        let body = format!("Acceptance:\n{}", "- item\n".repeat(40));
        assert_eq!(items(&body).len(), super::MAX_ACCEPTANCE);
    }
}
