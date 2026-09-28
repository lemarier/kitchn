//! A simulated Orca runtime at the process boundary.
//!
//! It answers the adapter's argument vectors with JSON shaped like Orca
//! 1.4.212's responses and can inject faults. Results from it are simulated
//! evidence about the adapter's mapping, never evidence about a live runtime.

#![allow(dead_code, reason = "each test uses a different subset")]

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimTask {
    pub id: String,
    pub title: String,
    pub status: &'static str,
    pub dispatch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SimAutomation {
    pub id: String,
    pub name: String,
    pub enabled: bool,
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
    pub mail: Value,
    pub faults: VecDeque<(Option<Vec<String>>, Fault)>,
    pub calls: Vec<Vec<String>>,
    pub deadlines: Vec<Duration>,
    /// Mutations that changed simulated state.
    pub effects: usize,
    pub stop_state: &'static str,
    pub release_action: &'static str,
    /// When set, `automations edit` is accepted but changes nothing.
    pub ignore_edits: bool,
    /// The coordinator terminal bound to the Run, once one was bound.
    pub bound: Option<String>,
    /// How the next `worker-start` ends: `ready` or `failed`.
    pub start_state: &'static str,
    next: u64,
}

/// The simulated runtime.
#[derive(Debug)]
pub struct SimOrca(Mutex<SimState>);

impl Default for SimOrca {
    fn default() -> Self {
        Self(Mutex::new(SimState {
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
            mail: json!({"deliveryId": null, "messages": [], "count": 0}),
            faults: VecDeque::new(),
            calls: Vec::new(),
            deadlines: Vec::new(),
            effects: 0,
            stop_state: "stopped",
            release_action: "released",
            ignore_edits: false,
            bound: None,
            start_state: "ready",
            next: 0,
        }))
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
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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

    pub fn set_worker(&self, dispatch: &str, worker: SimWorker) {
        self.state().workers.insert(dispatch.to_owned(), worker);
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
                // Orca prefixes the requested worktree name and reports the full ref.
                worker.branch = flags
                    .get("name")
                    .map(|name| format!("refs/heads/lemarier/{name}"));
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
                if !self.workers.contains_key(dispatch) {
                    return refuse("dispatch_not_found");
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
                let Some(worker) = self.workers.get_mut(&dispatch) else {
                    return refuse("dispatch_not_found");
                };
                // As Orca 1.4.212 does: the worker state records the stop,
                // the projected outcome reads `failed`, and the terminal is
                // released with it.
                if stop_state == "stopped" && worker.worker_state != "stopped" {
                    worker.worker_state = "stopped";
                    worker.outcome = "failed";
                    worker.liveness = "exited";
                    worker.release_state = "released";
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
                        "dispatch": {"id": dispatch, "runId": worker.run},
                        "worker": {
                            "state": worker.worker_state,
                            "effects": worker.worktree.iter().map(|id| json!({
                                "kind": "worktree", "action": "created_top_level", "id": id,
                            })).collect::<Vec<_>>(),
                        },
                        "projection": {
                            "outcome": worker.outcome,
                            "liveness": {"verdict": worker.liveness},
                        },
                        "observation": {
                            "status": "live",
                            "agentWait": if worker.waiting { json!({"source": "hook"}) } else { Value::Null },
                        },
                        "terminal": worker.branch.as_ref().map(|branch| json!({"branch": branch})),
                        "terminalResource": {
                            "releaseState": worker.release_state,
                            "ownershipState": worker.ownership,
                            "retainedReason": worker.retained_reason,
                        },
                    })),
                    None => refuse("dispatch_not_found"),
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
                self.bound = Some(Self::flag(flags, "from"));
                self.mutation(json!({"runId": Self::flag(flags, "id")}))
            }
            ["orchestration", "check"] => {
                let caller = Self::flag(flags, "terminal");
                if self.bound.as_ref().is_some_and(|bound| bound != &caller) {
                    return refuse("consumer_fenced");
                }
                ok(self.mail.clone())
            }
            ["automations", "list"] => ok(json!({
                "automations": self.automations.iter().map(|automation| json!({
                    "id": automation.id,
                    "name": automation.name,
                    "enabled": automation.enabled,
                    "prompt": "private prompt text",
                })).collect::<Vec<_>>()
            })),
            ["automations", "create"] => {
                let id = self.next_id("auto-");
                self.effects += 1;
                self.automations.push(SimAutomation {
                    id: id.clone(),
                    name: Self::flag(flags, "name"),
                    enabled: flags.contains_key("enabled"),
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
        let mut state = self.state();
        state.calls.push(invocation.args().to_vec());
        state.deadlines.push(invocation.deadline());
        let parsed = parse(invocation.args());
        let matching = state
            .faults
            .iter()
            .position(|(path, _)| path.as_ref().is_none_or(|path| path == &parsed.path));
        let fault = matching
            .and_then(|index| state.faults.remove(index))
            .map(|(_, fault)| fault);
        match fault {
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
        }
    }
}
