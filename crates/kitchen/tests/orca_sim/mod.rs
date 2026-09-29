//! A simulated Orca runtime at the process boundary.
//!
//! It answers the adapter's argument vectors with JSON shaped like Orca
//! 1.4.212's responses and can inject faults. Results from it are simulated
//! evidence about the adapter's mapping, never evidence about a live runtime.

#![allow(dead_code, reason = "each test uses a different subset")]

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use kitchen::adapters::orca::{Invocation, OrcaError, OrcaRunner, RawOutput};
use serde_json::{Value, json};

/// A fault applied to one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Nothing started.
    Spawn,
    /// Time out before Orca acts.
    TimeoutBeforeEffect,
    /// Act, then time out so the response is lost.
    TimeoutAfterEffect,
    /// Answer with an Orca error code.
    Refuse(&'static str),
    /// Print output that is not JSON.
    Garbage,
}

#[derive(Debug, Clone)]
pub struct SimWorker {
    pub worker_state: &'static str,
    pub outcome: &'static str,
    pub liveness: &'static str,
    pub waiting: bool,
    pub task: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub release_state: &'static str,
    pub ownership: &'static str,
    pub retained_reason: Option<&'static str>,
    pub run: &'static str,
    /// Orca's agent status in the projected stage.
    pub activity: &'static str,
    /// Overrides the stage detail derived from the worker state.
    pub stage_detail: Option<&'static str>,
    /// Overrides the Dispatch status derived from the outcome.
    pub dispatch_status: Option<&'static str>,
    /// Whether the Dispatch was fenced.
    pub fenced: bool,
    /// The start error Orca records.
    pub last_error: Option<&'static str>,
    /// The last line of terminal output Orca previews.
    pub preview: Option<&'static str>,
    /// What `worker-read` returns; Orca refuses the read when `None`.
    pub output: Option<SimOutput>,
    /// Whether the agent's terminal can still be shown. Orca reports the
    /// terminal, and its branch, only from the live terminal; the branch stays
    /// in the worktree record. A stop kept it on the live host (1.4.212); an
    /// exit is modelled as closing it.
    pub terminal_open: bool,
}

/// What `worker-read --source auto` returns.
#[derive(Debug, Clone)]
pub enum SimOutput {
    /// A proven provider transcript. `complete` is whether it fits the window.
    Transcript {
        messages: Vec<SimMessage>,
        complete: bool,
    },
    /// A released worker's archived transcript. As on 1.4.212, a read
    /// starts at the oldest archived message, and its cursor moves forward
    /// and never runs out: past the end, pages are empty.
    Archived(Vec<SimMessage>),
    /// The terminal tail, when there is no proven transcript.
    Terminal(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct SimMessage {
    pub role: &'static str,
    pub text: String,
    pub at: u64,
}

impl SimWorker {
    pub fn new(
        worker_state: &'static str,
        outcome: &'static str,
        liveness: &'static str,
        waiting: bool,
    ) -> Self {
        Self {
            worker_state,
            outcome,
            liveness,
            waiting,
            task: None,
            worktree: None,
            branch: None,
            release_state: "not_requested",
            ownership: "owned",
            retained_reason: None,
            run: "run_sim",
            activity: "unknown",
            stage_detail: None,
            dispatch_status: None,
            fenced: false,
            last_error: None,
            preview: None,
            output: None,
            terminal_open: true,
        }
    }

    /// The Dispatch status Orca derives from the outcome.
    fn dispatch_status(&self) -> &'static str {
        self.dispatch_status.unwrap_or(match self.outcome {
            "in_progress" => "dispatched",
            "succeeded" => "completed",
            _ => "failed",
        })
    }

    /// The stage detail Orca reports for the worker state.
    fn stage_detail(&self) -> Option<&'static str> {
        self.stage_detail.or(match self.worker_state {
            "ready" => Some("input_accepted"),
            "stopped" => Some("process_stopped"),
            "succeeded" | "failed" => Some("settled"),
            _ => None,
        })
    }

    fn rows(messages: &[SimMessage]) -> Vec<Value> {
        messages
            .iter()
            .map(|message| {
                json!({
                    "id": format!("m-{}", message.at),
                    "role": message.role,
                    "blocks": [{"type": "text", "text": message.text}],
                    "timestamp": message.at,
                    "source": "transcript",
                })
            })
            .collect()
    }

    fn archived(
        messages: &[SimMessage],
        offset: usize,
        limit: usize,
        cursor_ends_at: Option<usize>,
    ) -> Value {
        let page = messages.get(offset..).unwrap_or_default();
        let page = page.get(..limit.min(page.len())).unwrap_or_default();
        let window = Self::rows(page);
        let mut transcript = json!({
            "limited": messages.len() > limit,
            "returnedMessageCount": window.len(),
            "messages": window,
        });
        if cursor_ends_at.is_none_or(|end| offset < end) {
            transcript["nextCursor"] = json!(format!("a{}", offset + limit));
        }
        json!({
            "source": "transcript",
            "archived": true,
            "contentComplete": offset == 0 && messages.len() <= limit,
            "transcript": transcript,
        })
    }

    fn transcript(messages: &[SimMessage], complete: bool, limit: usize) -> Value {
        let start = messages.len().saturating_sub(limit);
        let window = Self::rows(messages.get(start..).unwrap_or_default());
        json!({
            "source": "transcript",
            "contentComplete": complete && start == 0,
            "transcript": {
                "limited": start > 0 || !complete,
                "returnedMessageCount": window.len(),
                "messages": window,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct SimTask {
    pub id: String,
    pub title: String,
    pub spec: String,
    pub status: &'static str,
    pub dispatch: Option<String>,
}

/// One automation. Its definition is the flags it was created with; the
/// listing derives Orca's stored fields from them, as `automations list`
/// shows them on 1.4.212 (`agentId`, `rrule`, `precheck.timeoutSeconds`, ...).
/// An automation built without flags has no readable definition.
#[derive(Debug, Clone, Default)]
pub struct SimAutomation {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub flags: BTreeMap<String, String>,
}

impl SimAutomation {
    fn listing(&self) -> Value {
        let flag = |name: &str| self.flags.get(name).map(String::as_str);
        let selector = |name: &str| {
            flag(name).map(|value| value.strip_prefix("id:").unwrap_or(value).to_owned())
        };
        let number = |name: &str| flag(name).and_then(|value| value.parse::<u64>().ok());
        let precheck = flag("precheck").map(
            |command| json!({"command": command, "timeoutSeconds": number("precheck-timeout")}),
        );
        json!({
            "id": self.id,
            "name": self.name,
            "enabled": self.enabled,
            "prompt": flag("prompt"),
            "agentId": flag("provider"),
            "timezone": flag("timezone"),
            "rrule": flag("trigger"),
            "missedRunGraceMinutes": number("missed-run-grace-minutes"),
            "reuseSession": self.flags.contains_key("reuse-session"),
            "precheck": precheck,
            "workspaceMode": flag("workspace-mode"),
            "workspaceId": selector("workspace"),
            "baseBranch": flag("base-branch"),
            "projectId": selector("repo"),
        })
    }
}

/// Holds the first `parties` callers of one command, after their calls took
/// effect, until all of them arrived or `wait` passed. It makes concurrent
/// callers interleave deterministically: each reads the state before any
/// of them writes.
#[derive(Debug, Clone)]
struct Gate {
    path: Vec<String>,
    parties: usize,
    wait: Duration,
    arrived: Arc<(Mutex<usize>, Condvar)>,
}

impl Gate {
    /// Count a caller in and return its arrival number.
    fn arrive(&self) -> usize {
        let (count, changed) = &*self.arrived;
        let mut count = count.lock().unwrap_or_else(PoisonError::into_inner);
        *count += 1;
        changed.notify_all();
        *count
    }

    /// Wait for the other parties, at most `wait`.
    fn hold(&self) {
        let (count, changed) = &*self.arrived;
        let mut count = count.lock().unwrap_or_else(PoisonError::into_inner);
        let end = Instant::now() + self.wait;
        while *count < self.parties {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            count = changed
                .wait_timeout(count, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[derive(Debug)]
pub struct SimState {
    pub version: &'static str,
    pub ready: bool,
    pub features: Vec<&'static str>,
    pub tasks: Vec<SimTask>,
    pub workers: BTreeMap<String, SimWorker>,
    pub automations: Vec<SimAutomation>,
    pub runs: Vec<Value>,
    pub worker_pages: Vec<Value>,
    /// Unacknowledged mailbox batches, oldest first. `check` returns the
    /// oldest; `--ack` consumes it only when it names that batch. A
    /// `run-use` from a terminal not yet bound gives each batch a new id.
    pub mail: VecDeque<Value>,
    pub faults: VecDeque<(Option<Vec<String>>, Fault)>,
    pub calls: Vec<Vec<String>>,
    pub deadlines: Vec<Duration>,
    /// Mutations that changed simulated state.
    pub effects: usize,
    pub stop_state: &'static str,
    /// Whether a stop also releases the worker's terminal, as on 1.4.212.
    pub stop_releases: bool,
    /// When set, `worker-read` answers this many more calls, then refuses
    /// with `source_changed`.
    pub reads_left: Option<usize>,
    /// Archived pages from this message offset on carry no `nextCursor`.
    pub archive_cursor_ends_at: Option<usize>,
    pub release_action: &'static str,
    /// When set, `automations edit` is accepted but changes nothing.
    pub ignore_edits: bool,
    /// The coordinator terminal bound to the Run, once one was bound.
    pub bound: Option<String>,
    /// Delivery ids replaced when another terminal adopted the Run.
    pub retired: Vec<String>,
    /// How the next `worker-start` ends: `ready` or `failed`.
    pub start_state: &'static str,
    /// The prefix Orca puts before the name of a worktree it creates.
    pub branch_prefix: &'static str,
    /// The branch an existing worktree is on, when Orca reports one.
    pub existing_branch: Option<&'static str>,
    /// Worktrees Orca lists that no simulated worker created: id and branch.
    pub worktrees: Vec<(String, String)>,
    /// Branches that exist in Git without an Orca worktree.
    pub git_branches: Vec<String>,
    /// Whether `worktree list` reports its listing truncated.
    pub listing_truncated: bool,
    /// Worktrees `worktree list` counts in `totalCount` but does not return.
    pub unlisted_worktrees: usize,
    /// Hosts `worktree list` reports it did not cover.
    pub omitted_hosts: Vec<&'static str>,
    /// When set, the worker settles in this state just before Orca handles
    /// the next `worker-stop`, as when it exits while the stop is sent.
    pub settle_before_stop: Option<&'static str>,
    gate: Option<Gate>,
    next: u64,
}

/// The simulated runtime, with the runtime directory its backends reserve
/// keys in.
#[derive(Debug)]
pub struct SimOrca {
    state: Mutex<SimState>,
    dir: Option<tempfile::TempDir>,
}

impl Default for SimOrca {
    fn default() -> Self {
        Self {
            dir: tempfile::tempdir().ok(),
            state: Mutex::new(SimState {
                version: "1.4.212",
                ready: true,
                features: vec![
                    "orchestration.contract.v1",
                    "orchestration.worker-stop-verdict.v1",
                ],
                tasks: Vec::new(),
                workers: BTreeMap::new(),
                automations: Vec::new(),
                runs: Vec::new(),
                worker_pages: Vec::new(),
                mail: VecDeque::new(),
                faults: VecDeque::new(),
                calls: Vec::new(),
                deadlines: Vec::new(),
                effects: 0,
                stop_state: "stopped",
                stop_releases: true,
                reads_left: None,
                archive_cursor_ends_at: None,
                release_action: "released",
                ignore_edits: false,
                bound: None,
                retired: Vec::new(),
                start_state: "ready",
                branch_prefix: "lemarier/",
                existing_branch: None,
                worktrees: Vec::new(),
                git_branches: Vec::new(),
                listing_truncated: false,
                unlisted_worktrees: 0,
                omitted_hosts: Vec::new(),
                settle_before_stop: None,
                gate: None,
                next: 0,
            }),
        }
    }
}

fn ok(result: Value) -> RawOutput {
    RawOutput {
        exit_code: Some(0),
        stdout: json!({"id": "sim", "ok": true, "result": result})
            .to_string()
            .into_bytes(),
    }
}

fn refuse(code: &str) -> RawOutput {
    RawOutput {
        exit_code: Some(1),
        stdout: json!({"id": "sim", "ok": false, "error": {"code": code, "message": "refused"}})
            .to_string()
            .into_bytes(),
    }
}

struct Parsed {
    path: Vec<String>,
    flags: BTreeMap<String, String>,
}

/// Orca's `task-list --brief` form of a spec: whitespace collapsed to single
/// spaces, then capped at 160 characters.
fn brief_spec(spec: &str) -> String {
    spec.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

fn parse(args: &[String]) -> Parsed {
    let mut path = Vec::new();
    let mut flags = BTreeMap::new();
    for arg in args {
        match arg.strip_prefix("--") {
            Some(flag) => match flag.split_once('=') {
                Some((name, value)) => {
                    flags.insert(name.to_owned(), value.to_owned());
                }
                None => {
                    flags.insert(flag.to_owned(), String::new());
                }
            },
            None => path.push(arg.clone()),
        }
    }
    Parsed { path, flags }
}

impl SimOrca {
    pub fn state(&self) -> MutexGuard<'_, SimState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The directory backends of this simulator share for reservations.
    pub fn runtime_dir(&self) -> Result<std::path::PathBuf, &'static str> {
        self.dir
            .as_ref()
            .map(|dir| dir.path().to_path_buf())
            .ok_or("no temporary directory")
    }

    /// Apply `fault` to the next call of any command.
    pub fn fault(&self, fault: Fault) {
        self.state().faults.push_back((None, fault));
    }

    /// Apply `fault` to the next call of the command at `path`.
    pub fn fault_on(&self, path: &[&str], fault: Fault) {
        let path = path.iter().map(|part| (*part).to_owned()).collect();
        self.state().faults.push_back((Some(path), fault));
    }

    /// Hold the first `parties` calls of the command at `path` until all
    /// of them were made (or `wait` passed), so they read state before any
    /// of them writes.
    pub fn interleave(&self, path: &[&str], parties: usize, wait: Duration) {
        self.state().gate = Some(Gate {
            path: path.iter().map(|part| (*part).to_owned()).collect(),
            parties,
            wait,
            arrived: Arc::new((Mutex::new(0), Condvar::new())),
        });
    }

    pub fn set_worker(&self, dispatch: &str, worker: SimWorker) {
        self.state().workers.insert(dispatch.to_owned(), worker);
    }

    /// End a worker's process without a stop, as Orca 1.4.212 records it
    /// (`failDispatch` with `workerProcessExited`): the Dispatch and worker
    /// fail at stage `process_exited`, and the Task returns to `ready` so
    /// a new Dispatch may start it. `dispatch-show --task` still names the
    /// failed Dispatch.
    pub fn exit_worker(&self, dispatch: &str) -> Result<(), &'static str> {
        let mut state = self.state();
        let worker = state.workers.get_mut(dispatch).ok_or("no such worker")?;
        worker.worker_state = "failed";
        worker.outcome = "failed";
        worker.liveness = "exited";
        worker.stage_detail = Some("process_exited");
        worker.dispatch_status = Some("failed");
        worker.fenced = true;
        worker.terminal_open = false;
        let task = worker.task.clone().ok_or("the worker has no Task")?;
        let task = state
            .tasks
            .iter_mut()
            .find(|candidate| candidate.id == task)
            .ok_or("no such Task")?;
        task.status = "ready";
        Ok(())
    }

    /// Calls whose command path starts with `path`.
    pub fn calls_to(&self, path: &[&str]) -> Vec<Vec<String>> {
        self.state()
            .calls
            .iter()
            .filter(|call| call.len() >= path.len() && call.iter().zip(path).all(|(a, b)| a == b))
            .cloned()
            .collect()
    }
}

impl SimState {
    fn next_id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    /// Answer a mutation the way Orca does: with its own request id.
    fn mutation(&mut self, mut result: Value) -> RawOutput {
        let request = self.next_id("req-");
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "mutation".to_owned(),
                json!({"requestId": request, "replayed": false}),
            );
        }
        ok(result)
    }

    /// Whether `branch` exists, in an Orca worktree or only in Git.
    fn branch_exists(&self, branch: &str) -> bool {
        let full = format!("refs/heads/{branch}");
        self.workers
            .values()
            .any(|worker| worker.branch.as_deref() == Some(full.as_str()))
            || self.worktrees.iter().any(|(_, listed)| listed == &full)
            || self.git_branches.iter().any(|existing| existing == branch)
    }

    /// The full ref Orca creates for `branch`: as asked, or with the first
    /// free numeric suffix from 2 when it exists (a collision).
    fn free_branch(&self, branch: &str) -> String {
        let free = std::iter::once(branch.to_owned())
            .chain((2..).map(|n| format!("{branch}-{n}")))
            .find(|candidate| !self.branch_exists(candidate))
            .unwrap_or_default();
        format!("refs/heads/{free}")
    }

    fn flag(flags: &BTreeMap<String, String>, name: &str) -> String {
        flags.get(name).cloned().unwrap_or_default()
    }

    fn handle(&mut self, parsed: &Parsed) -> RawOutput {
        let flags = &parsed.flags;
        let path: Vec<&str> = parsed.path.iter().map(String::as_str).collect();
        match path.as_slice() {
            ["status"] => ok(json!({
                "runtime": {
                    "state": if self.ready { "ready" } else { "starting" },
                    "reachable": self.ready,
                    "appVersion": self.version,
                    "capabilities": self.features,
                }
            })),
            ["orchestration", "task-create"] => {
                let id = self.next_id("task_");
                self.effects += 1;
                self.tasks.push(SimTask {
                    id: id.clone(),
                    // Orca truncates titles to 80 characters.
                    title: Self::flag(flags, "task-title").chars().take(80).collect(),
                    spec: Self::flag(flags, "spec"),
                    status: "ready",
                    dispatch: None,
                });
                self.mutation(json!({"task": {"id": id, "status": "ready"}}))
            }
            ["orchestration", "task-list"] => ok(json!({
                "runId": "run_sim",
                "tasks": self.tasks.iter().map(|task| json!({
                    "id": task.id,
                    "task_title": task.title,
                    "spec": if flags.contains_key("brief") {
                        brief_spec(&task.spec)
                    } else {
                        task.spec.clone()
                    },
                    "status": task.status,
                })).collect::<Vec<_>>(),
            })),
            ["orchestration", "dispatch-show"] => {
                let task = Self::flag(flags, "task");
                match self.tasks.iter().find(|candidate| candidate.id == task) {
                    Some(task) => ok(json!({
                        "dispatch": task.dispatch.as_ref().map(|id| json!({"id": id})),
                    })),
                    None => refuse("task_not_found"),
                }
            }
            ["orchestration", "worker-start"] => {
                let task_id = Self::flag(flags, "task");
                let Some(index) = self.tasks.iter().position(|task| task.id == task_id) else {
                    return refuse("task_not_found");
                };
                if self.tasks.get(index).map(|task| task.status) != Some("ready") {
                    return refuse("task_not_startable");
                }
                let dispatch = self.next_id("ctx_");
                let worktree = self.next_id("wt_");
                self.effects += 1;
                if let Some(task) = self.tasks.get_mut(index) {
                    task.status = "dispatched";
                    task.dispatch = Some(dispatch.clone());
                }
                let start_state = self.start_state;
                let mut worker = if start_state == "ready" {
                    SimWorker::new("ready", "in_progress", "live", false)
                } else {
                    SimWorker::new("failed", "failed", "exited", false)
                };
                worker.task = Some(task_id.clone());
                worker.worktree = Some(worktree.clone());
                // Orca prefixes the requested worktree name and reports the
                // full ref; an existing worktree keeps the branch it has.
                worker.branch = match flags.get("name") {
                    Some(name) => Some(self.free_branch(&format!("{}{name}", self.branch_prefix))),
                    None => self
                        .existing_branch
                        .map(|branch| format!("refs/heads/{branch}")),
                };
                self.workers.insert(dispatch.clone(), worker);
                self.mutation(json!({
                    "runId": "run_sim",
                    "taskId": task_id,
                    "dispatchId": dispatch,
                    "state": start_state,
                    "stage": "input_accepted",
                    "effects": [{"kind": "worktree", "action": "created_top_level", "id": worktree}],
                    "residualResources": [],
                }))
            }
            ["orchestration", "send"] => {
                let to = Self::flag(flags, "to");
                let dispatch = to.strip_prefix("dispatch:").unwrap_or_default();
                let Some(worker) = self.workers.get(dispatch) else {
                    return refuse("dispatch_not_found");
                };
                // As Orca 1.4.212 does: a Dispatch that is no longer active
                // will never read its mailbox, and nothing is queued.
                if worker.dispatch_status() != "dispatched" || worker.fenced {
                    return refuse("dispatch_inactive");
                }
                self.effects += 1;
                let message = self.next_id("msg_");
                self.mutation(json!({"messageId": message}))
            }
            ["orchestration", "reply"] => {
                self.effects += 1;
                let message = self.next_id("msg_");
                self.mutation(json!({"messageId": message}))
            }
            ["orchestration", "worker-stop"] => {
                let dispatch = Self::flag(flags, "dispatch");
                let stop_state = self.stop_state;
                let stop_releases = self.stop_releases;
                let settle = self.settle_before_stop.take();
                let Some(worker) = self.workers.get_mut(&dispatch) else {
                    return refuse("dispatch_not_found");
                };
                if let Some(settled) = settle {
                    worker.worker_state = settled;
                    worker.outcome = settled;
                    worker.liveness = "exited";
                }
                // As Orca 1.4.212 does (`wMn`): a settled worker is not
                // stopped again; the answer names the state it settled in.
                if matches!(
                    worker.worker_state,
                    "stopped" | "failed" | "succeeded" | "abandoned"
                ) {
                    let settled = worker.worker_state;
                    return self.mutation(json!({
                        "dispatchId": dispatch,
                        "state": settled,
                        "alreadySettled": true,
                        "processAction": "none",
                    }));
                }
                // As Orca 1.4.212 does: the worker state records the stop,
                // the projected outcome reads `failed`, and the terminal is
                // released with it.
                if stop_state == "stopped" {
                    worker.worker_state = "stopped";
                    worker.outcome = "failed";
                    worker.liveness = "exited";
                    if stop_releases {
                        worker.release_state = "released";
                    }
                    self.effects += 1;
                }
                self.mutation(json!({"dispatchId": dispatch, "state": stop_state}))
            }
            ["orchestration", "worker-release"] => {
                let dispatch = Self::flag(flags, "dispatch");
                let action = self.release_action;
                let Some(worker) = self.workers.get_mut(&dispatch) else {
                    return refuse("dispatch_not_found");
                };
                if action == "released" {
                    worker.release_state = "released";
                    self.effects += 1;
                }
                self.mutation(
                    json!({"dispatchId": dispatch, "state": action, "processAction": "none"}),
                )
            }
            ["orchestration", "worker-show"] => {
                let dispatch = Self::flag(flags, "dispatch");
                match self.workers.get(&dispatch) {
                    Some(worker) => ok(json!({
                        "dispatch": {
                            "id": dispatch,
                            "runId": worker.run,
                            "status": worker.dispatch_status(),
                            "capabilityRevokedAt": worker.fenced.then_some("2026-09-28T16:00:00Z"),
                        },
                        "worker": {
                            "state": worker.worker_state,
                            "stage": worker.stage_detail(),
                            "lastError": worker.last_error,
                            "effects": worker.worktree.iter().map(|id| json!({
                                "kind": "worktree", "action": "created_top_level", "id": id,
                            })).collect::<Vec<_>>(),
                        },
                        "projection": {
                            "outcome": worker.outcome,
                            "liveness": {"verdict": worker.liveness},
                            "stage": {
                                "worker": worker.worker_state,
                                "dispatch": worker.dispatch_status(),
                                "detail": worker.stage_detail(),
                                "activity": worker.activity,
                            },
                        },
                        "observation": {
                            "status": "live",
                            "agentWait": if worker.waiting { json!({"source": "hook"}) } else { Value::Null },
                        },
                        "terminal": (worker.terminal_open
                            && (worker.branch.is_some() || worker.preview.is_some()))
                            .then(|| json!({"branch": worker.branch, "preview": worker.preview})),
                        "terminalResource": {
                            "releaseState": worker.release_state,
                            "ownershipState": worker.ownership,
                            "retainedReason": worker.retained_reason,
                        },
                    })),
                    None => refuse("dispatch_not_found"),
                }
            }
            ["worktree", "show"] => {
                let selector = Self::flag(flags, "worktree");
                let id = selector.strip_prefix("id:").unwrap_or_default();
                match self
                    .workers
                    .values()
                    .find(|worker| worker.worktree.as_deref() == Some(id))
                {
                    Some(worker) => ok(json!({
                        "worktree": {"id": id, "branch": worker.branch},
                    })),
                    None => refuse("worktree_not_found"),
                }
            }
            ["worktree", "list"] => {
                let rows: Vec<Value> = self
                    .workers
                    .values()
                    .filter_map(|worker| {
                        worker
                            .worktree
                            .as_ref()
                            .map(|id| json!({"id": id, "branch": worker.branch}))
                    })
                    .chain(
                        self.worktrees
                            .iter()
                            .map(|(id, branch)| json!({"id": id, "branch": branch})),
                    )
                    .collect();
                ok(json!({
                    "worktrees": rows,
                    "hostScope": {"hostIds": ["local"], "omittedHostIds": self.omitted_hosts},
                    "totalCount": rows.len() + self.unlisted_worktrees,
                    "truncated": self.listing_truncated,
                }))
            }
            ["orchestration", "worker-read"] => {
                let dispatch = Self::flag(flags, "dispatch");
                let limit = flags
                    .get("limit")
                    .and_then(|limit| limit.parse().ok())
                    .unwrap_or(50);
                match self.reads_left {
                    Some(0) => return refuse("source_changed"),
                    Some(left) => self.reads_left = Some(left - 1),
                    None => {}
                }
                let Some(worker) = self.workers.get(&dispatch) else {
                    return refuse("dispatch_not_found");
                };
                match &worker.output {
                    None => refuse("worker_read_unavailable"),
                    Some(SimOutput::Transcript { messages, complete }) => {
                        ok(SimWorker::transcript(messages, *complete, limit))
                    }
                    Some(SimOutput::Archived(messages)) => {
                        let offset = match flags.get("cursor") {
                            None => 0,
                            Some(cursor) => {
                                match cursor.strip_prefix('a').and_then(|n| n.parse().ok()) {
                                    Some(offset) => offset,
                                    None => return refuse("invalid_cursor"),
                                }
                            }
                        };
                        ok(SimWorker::archived(
                            messages,
                            offset,
                            limit,
                            self.archive_cursor_ends_at,
                        ))
                    }
                    Some(SimOutput::Terminal(lines)) => ok(json!({
                        "source": "terminal",
                        "fallbackReason": "no_transcript",
                        "contentComplete": false,
                        "terminal": {"tail": lines},
                    })),
                }
            }
            ["orchestration", "worker-list"] => {
                if self.worker_pages.is_empty() {
                    return ok(json!({
                        "workers": self.workers.iter().map(|(dispatch, worker)| json!({
                            "dispatchId": dispatch,
                            "taskId": worker.task,
                            "workerState": worker.worker_state,
                            "terminalState": if worker.retained_reason.is_some() { "retained" } else { "active" },
                            "resource": {
                                "ownershipState": worker.ownership,
                                "retainedReason": worker.retained_reason,
                            },
                            "projection": {
                                "outcome": worker.outcome,
                                "liveness": {"verdict": worker.liveness},
                            },
                        })).collect::<Vec<_>>(),
                        "page": {"hasMore": false},
                    }));
                }
                let index = match flags.get("cursor") {
                    None => 0,
                    Some(cursor) => match cursor.strip_prefix('p').and_then(|n| n.parse().ok()) {
                        Some(index) => index,
                        None => return refuse("invalid_cursor"),
                    },
                };
                ok(self
                    .worker_pages
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| json!({"workers": [], "page": {"hasMore": false}})))
            }
            ["orchestration", "run-use"] => {
                let from = Self::flag(flags, "from");
                // A new coordinator gets every unacknowledged message again,
                // under a new delivery id, as live Orca 1.4.216 does.
                if self.bound.as_ref() != Some(&from) {
                    for index in 0..self.mail.len() {
                        let id = self.next_id("delivery_redelivered_");
                        if let Some(batch) = self.mail.get_mut(index) {
                            if let Some(old) = batch["deliveryId"].as_str() {
                                self.retired.push(old.to_owned());
                            }
                            batch["deliveryId"] = json!(id);
                        }
                    }
                }
                self.bound = Some(from);
                self.mutation(json!({"runId": Self::flag(flags, "id")}))
            }
            ["orchestration", "check"] => {
                let caller = Self::flag(flags, "terminal");
                if self.bound.as_ref().is_some_and(|bound| bound != &caller) {
                    return refuse("consumer_fenced");
                }
                let ack = flags.get("ack").map(String::as_str);
                // As 1.4.216 answers an acknowledgement of a batch id issued
                // before the caller adopted the Run.
                if ack.is_some_and(|ack| self.retired.iter().any(|old| old == ack)) {
                    return refuse("consumer_fenced");
                }
                if ack.is_some()
                    && self
                        .mail
                        .front()
                        .and_then(|batch| batch["deliveryId"].as_str())
                        == ack
                {
                    self.mail.pop_front();
                }
                ok(self
                    .mail
                    .front()
                    .cloned()
                    .unwrap_or_else(|| json!({"deliveryId": null, "messages": [], "count": 0})))
            }
            ["automations", "list"] => ok(json!({
                "automations": self.automations.iter().map(SimAutomation::listing).collect::<Vec<_>>()
            })),
            ["automations", "create"] => {
                let id = self.next_id("auto-");
                self.effects += 1;
                self.automations.push(SimAutomation {
                    id: id.clone(),
                    name: Self::flag(flags, "name"),
                    enabled: flags.contains_key("enabled"),
                    flags: flags.clone(),
                });
                ok(json!({"automation": {"id": id, "name": flags.get("name"), "enabled": false}}))
            }
            ["automations", "edit"] => {
                let id = Self::flag(flags, "id");
                let ignore = self.ignore_edits;
                let Some(automation) = self.automations.iter_mut().find(|a| a.id == id) else {
                    return refuse("automation_not_found");
                };
                if !ignore {
                    if flags.contains_key("enabled") {
                        automation.enabled = true;
                    }
                    if flags.contains_key("disabled") {
                        automation.enabled = false;
                    }
                    self.effects += 1;
                }
                ok(json!({"id": id}))
            }
            ["automations", "remove"] => {
                let id = Self::flag(flags, "id");
                self.effects += 1;
                self.automations.retain(|automation| automation.id != id);
                ok(json!({"removed": true}))
            }
            ["automations", "run"] => {
                self.effects += 1;
                ok(json!({"runId": self.next_id("autorun-")}))
            }
            ["automations", "runs"] => ok(json!({"runs": self.runs})),
            _ => refuse("unknown_command"),
        }
    }
}

impl OrcaRunner for SimOrca {
    fn run(&self, invocation: &Invocation) -> Result<RawOutput, OrcaError> {
        let parsed = parse(invocation.args());
        let (result, held) = {
            let mut state = self.state();
            state.calls.push(invocation.args().to_vec());
            state.deadlines.push(invocation.deadline());
            let matching = state
                .faults
                .iter()
                .position(|(path, _)| path.as_ref().is_none_or(|path| path == &parsed.path));
            let fault = matching
                .and_then(|index| state.faults.remove(index))
                .map(|(_, fault)| fault);
            let result = match fault {
                None => Ok(state.handle(&parsed)),
                Some(Fault::Spawn) => Err(OrcaError::Spawn(std::io::ErrorKind::NotFound)),
                Some(Fault::TimeoutBeforeEffect) => Err(OrcaError::Timeout),
                Some(Fault::TimeoutAfterEffect) => {
                    let _ = state.handle(&parsed);
                    Err(OrcaError::Timeout)
                }
                Some(Fault::Refuse(code)) => Ok(refuse(code)),
                Some(Fault::Garbage) => Ok(RawOutput {
                    exit_code: Some(0),
                    stdout: b"Orca is updating...".to_vec(),
                }),
            };
            let held = state
                .gate
                .as_ref()
                .filter(|gate| gate.path == parsed.path)
                .filter(|gate| gate.arrive() <= gate.parties)
                .cloned();
            (result, held)
        };
        // Wait outside the state lock so the other callers can arrive.
        if let Some(gate) = held {
            gate.hold();
        }
        result
    }
}
