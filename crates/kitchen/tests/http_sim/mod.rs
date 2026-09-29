//! A local fake of an HTTP worker protocol service.
//!
//! It listens on a loopback port and answers each call from the in-memory
//! [`FakeBackend`], which owns every contract rule, so the tests exercise the
//! adapter's wire mapping, transport, and local refusals. Responses are
//! written by hand here rather than through the adapter's wire types, so the
//! two sides agree only through the documented JSON. It records every
//! request and can inject faults. Results are simulated evidence, never live
//! runtime evidence.

#![allow(dead_code, reason = "each test uses a different subset")]

use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use kitchen::{
    contracts::{
        Capability, CoordinatorMailbox, Delivery, EffectExecutor, EffectFailure, EffectRequest,
        ExternalRef, Liveness, Lookup, MailboxError, MessageKind, NotAppliedReason, ResourceRef,
        Support, WorkerBackend, WorkerOutcome, WorkerState, fake::FakeBackend,
    },
    selection::EffortSupport,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// A fault for the next request that reaches the router.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Answer with this status and an empty body, without acting.
    Status(u16),
    /// Answer 200 with a body that is not the protocol's JSON, after acting.
    GarbageAfterActing,
    /// Wait this long before handling, so the client's deadline passes.
    Delay(Duration),
    /// Close the connection without answering, after acting.
    DropAfterActing,
    /// Answer with this exact status and body, without acting.
    Raw(u16, String),
}

/// One request as the service received it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub idempotency_key: Option<String>,
    pub body: String,
}

struct State {
    token: String,
    descriptor: Value,
    /// One fake instance per coordinator, as after a restart.
    coordinators: BTreeMap<String, Arc<FakeBackend>>,
    usage: BTreeMap<String, Value>,
    faults: VecDeque<Fault>,
    requests: Vec<Recorded>,
    /// Await calls answer `empty` at once, as a service that does not hold.
    await_calls: usize,
}

/// The running fake service. Dropping it stops the listener.
pub struct SimHttp {
    addr: SocketAddr,
    backend: Arc<FakeBackend>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

impl SimHttp {
    /// Serve `backend` with bearer `token`, reporting `backend`'s descriptor.
    pub fn start(backend: FakeBackend, token: &str) -> TestResult<Self> {
        let descriptor = descriptor_json(&backend);
        Self::start_with(backend, token, descriptor)
    }

    /// Serve `backend`, reporting `descriptor` as the service's own.
    pub fn start_with(backend: FakeBackend, token: &str, descriptor: Value) -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let backend = Arc::new(backend);
        let state = Arc::new(Mutex::new(State {
            token: token.to_owned(),
            descriptor,
            coordinators: BTreeMap::new(),
            usage: BTreeMap::new(),
            faults: VecDeque::new(),
            requests: Vec::new(),
            await_calls: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let backend = Arc::clone(&backend);
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        // A broken client connection only fails that call.
                        let _ = serve(stream, &backend, &state);
                    }
                }
            })
        };
        Ok(Self {
            addr,
            backend,
            state,
            stop,
            thread: Some(thread),
        })
    }

    /// The service's base URL.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// The fake behind the service, for seeding and inspection.
    pub fn backend(&self) -> &FakeBackend {
        &self.backend
    }

    /// Queue a fault for the next routed request.
    pub fn inject(&self, fault: Fault) {
        lock(&self.state).faults.push_back(fault);
    }

    /// Report `report` as the usage of the worker with `handle`.
    pub fn set_usage(&self, handle: &str, report: Value) {
        lock(&self.state).usage.insert(handle.to_owned(), report);
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.state).requests.clone()
    }

    /// How many await calls were answered.
    pub fn await_calls(&self) -> usize {
        lock(&self.state).await_calls
    }
}

impl Drop for SimHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The descriptor JSON a service reports for `backend`.
pub fn descriptor_json(backend: &FakeBackend) -> Value {
    let descriptor = backend.descriptor();
    let capabilities: serde_json::Map<String, Value> = Capability::ALL
        .into_iter()
        .filter_map(|capability| {
            let support = match descriptor.capabilities.support(capability)? {
                Support::Supported => "supported",
                Support::Partial => "partial",
            };
            Some((capability.as_str().to_owned(), json!(support)))
        })
        .collect();
    let selection = descriptor.worker_selection.map(|selection| {
        json!({
            "families": selection.families.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
            "model": selection.model,
            "effort": match selection.effort {
                EffortSupport::Unsupported => "unsupported",
                EffortSupport::WithModel => "with-model",
                EffortSupport::Always => "always",
            },
        })
    });
    json!({
        "backend": descriptor.backend,
        "house": descriptor.house,
        "capabilities": capabilities,
        "workerSelection": selection,
    })
}

fn serve(stream: TcpStream, backend: &FakeBackend, state: &Mutex<State>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let mut stream = stream;
    if headers
        .get("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    let recorded = Recorded {
        method,
        path,
        authorization: headers.get("authorization").cloned(),
        idempotency_key: headers.get("idempotency-key").cloned(),
        body,
    };
    let (fault, token) = {
        let mut state = lock(state);
        state.requests.push(recorded.clone());
        (state.faults.pop_front(), state.token.clone())
    };
    if let Some(Fault::Delay(delay)) = &fault {
        thread::sleep(*delay);
    }
    let answer = match &fault {
        Some(Fault::Status(status)) => Some((*status, String::new())),
        Some(Fault::Raw(status, body)) => Some((*status, body.clone())),
        _ => None,
    };
    let (status, reply) = match answer {
        Some(answer) => answer,
        None if recorded.authorization.as_deref() != Some(&format!("Bearer {token}")) => {
            (401, String::new())
        }
        None => route(&recorded, backend, state),
    };
    match fault {
        Some(Fault::GarbageAfterActing) => respond(&mut stream, 200, "<html>proxy</html>"),
        Some(Fault::DropAfterActing) => Ok(()),
        _ => respond(&mut stream, status, &reply),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[derive(Deserialize)]
struct EffectCall {
    #[allow(dead_code, reason = "the fake serves one run")]
    run: ExternalRef,
    request: EffectRequest,
}

#[derive(Deserialize)]
struct WorkerCall {
    worker: ResourceRef,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxCall {
    #[allow(dead_code, reason = "the fake serves one run")]
    run: ExternalRef,
    coordinator: String,
    #[serde(default)]
    delivery: Option<ExternalRef>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

fn route(call: &Recorded, backend: &FakeBackend, state: &Mutex<State>) -> (u16, String) {
    let bad = (400, String::new());
    let ok = |value: Value| (200, value.to_string());
    match (call.method.as_str(), call.path.as_str()) {
        ("GET", "/v1/descriptor") => ok(lock(state).descriptor.clone()),
        ("POST", "/v1/effects") => {
            let Ok(effect) = serde_json::from_str::<EffectCall>(&call.body) else {
                return bad;
            };
            match backend.execute(&effect.request) {
                Ok(receipt) => ok(json!({ "status": "applied", "receipt": receipt })),
                Err(EffectFailure::NotApplied(reason)) => ok(refusal(reason)),
                Err(EffectFailure::Uncertain(_)) => (502, String::new()),
            }
        }
        ("POST", "/v1/effects/lookup") => {
            let Ok(effect) = serde_json::from_str::<EffectCall>(&call.body) else {
                return bad;
            };
            match backend.lookup(&effect.request) {
                Ok(Lookup::Applied(receipt)) => {
                    ok(json!({ "status": "applied", "receipt": receipt }))
                }
                Ok(Lookup::Absent) => ok(json!({ "status": "absent" })),
                Ok(Lookup::Unknown) => ok(json!({ "status": "unknown" })),
                Err(_) => (503, String::new()),
            }
        }
        ("POST", "/v1/workers/observe") => {
            let Ok(worker) = serde_json::from_str::<WorkerCall>(&call.body) else {
                return bad;
            };
            match backend.observe_worker(&worker.worker) {
                Ok(state) => ok(state_json(state)),
                Err(_) => (503, String::new()),
            }
        }
        ("GET", "/v1/inventory") => match backend.inventory() {
            Ok(listed) => ok(json!({
                "resources": listed.iter().map(|observation| json!({
                    "resource": observation.resource,
                    "owner": observation.owner,
                    "liveness": match observation.liveness {
                        Liveness::Live => "live",
                        Liveness::Exited => "exited",
                        Liveness::Unverifiable => "unverifiable",
                    },
                })).collect::<Vec<_>>(),
            })),
            Err(_) => (503, String::new()),
        },
        ("POST", "/v1/workers/usage") => {
            let Ok(worker) = serde_json::from_str::<WorkerCall>(&call.body) else {
                return bad;
            };
            match lock(state).usage.get(worker.worker.handle.as_str()) {
                Some(report) => ok(json!({ "status": "reported", "report": report })),
                None => ok(json!({ "status": "none" })),
            }
        }
        ("POST", path) if path == "/v1/runs/adopt" || path.starts_with("/v1/deliveries/") => {
            let Ok(mailbox) = serde_json::from_str::<MailboxCall>(&call.body) else {
                return bad;
            };
            let instance = {
                let mut state = lock(state);
                if path == "/v1/deliveries/await" {
                    state.await_calls = state.await_calls.saturating_add(1);
                }
                Arc::clone(
                    state
                        .coordinators
                        .entry(mailbox.coordinator.clone())
                        .or_insert_with(|| Arc::new(backend.restarted())),
                )
            };
            let answer = match path {
                "/v1/runs/adopt" => instance.adopt_run().map(|()| None),
                "/v1/deliveries/next" => instance.next_delivery(),
                "/v1/deliveries/acknowledge" => match &mailbox.delivery {
                    Some(delivery) => instance.acknowledge(delivery),
                    None => return bad,
                },
                "/v1/deliveries/await" => instance
                    .await_delivery(Duration::from_millis(mailbox.wait_ms.unwrap_or_default())),
                _ => return (404, String::new()),
            };
            match answer {
                Ok(_) if path == "/v1/runs/adopt" => ok(json!({})),
                Ok(Some(delivery)) => ok(json!({
                    "status": "delivery",
                    "delivery": delivery_json(&delivery),
                })),
                Ok(None) => ok(json!({ "status": "empty" })),
                Err(MailboxError::Fenced) => ok(json!({ "status": "fenced" })),
                Err(MailboxError::Unavailable(_)) => (503, String::new()),
            }
        }
        _ => (404, String::new()),
    }
}

fn refusal(reason: NotAppliedReason) -> Value {
    match reason {
        NotAppliedReason::Unsupported(capability) => {
            json!({ "status": "not-applied", "reason": "unsupported", "capability": capability })
        }
        NotAppliedReason::CrossHouse => json!({ "status": "not-applied", "reason": "cross-house" }),
        NotAppliedReason::ForeignBackend => {
            json!({ "status": "not-applied", "reason": "foreign-backend" })
        }
        NotAppliedReason::RateLimited { retry_after } => json!({
            "status": "not-applied",
            "reason": "rate-limited",
            "retryAfterSeconds": retry_after.map(|delay| delay.as_secs()),
        }),
        NotAppliedReason::Rejected | NotAppliedReason::ConfirmedAbsent => {
            json!({ "status": "not-applied", "reason": "rejected" })
        }
    }
}

fn state_json(state: WorkerState) -> Value {
    match state {
        WorkerState::Starting => json!({ "state": "starting" }),
        WorkerState::Ready => json!({ "state": "ready" }),
        WorkerState::AwaitingReply => json!({ "state": "awaiting-reply" }),
        WorkerState::UserTakeover => json!({ "state": "user-takeover" }),
        WorkerState::Settled(outcome) => json!({
            "state": "settled",
            "outcome": match outcome {
                WorkerOutcome::Succeeded => "succeeded",
                WorkerOutcome::Failed => "failed",
                WorkerOutcome::Cancelled => "cancelled",
            },
        }),
        WorkerState::Missing => json!({ "state": "missing" }),
        WorkerState::Unknown => json!({ "state": "unknown" }),
    }
}

fn delivery_json(delivery: &Delivery) -> Value {
    json!({
        "id": delivery.id,
        "unreadable": delivery.unreadable,
        "messages": delivery.messages.iter().map(|message| json!({
            "id": message.id,
            "kind": match message.kind {
                MessageKind::Question => "question",
                MessageKind::WorkerDone => "worker-done",
                MessageKind::Escalation => "escalation",
                MessageKind::Heartbeat => "heartbeat",
                MessageKind::Status => "status",
                MessageKind::Other => "note",
            },
            "worker": message.worker,
            "outcome": message.outcome.map(|outcome| match outcome {
                WorkerOutcome::Succeeded => "succeeded",
                WorkerOutcome::Failed => "failed",
                WorkerOutcome::Cancelled => "cancelled",
            }),
            "subject": message.subject.as_ref().map(|text| text.as_str()),
            "body": message.body.as_ref().map(|text| text.as_str()),
        })).collect::<Vec<_>>(),
    })
}
