//! The HTTP worker backend (#195) against a local fake service (`http_sim`):
//! the executor, worker, and mailbox conformance suites; typed not-applied
//! versus uncertain outcomes for each transport and status failure;
//! capability declarations and activation refusals through the resolver;
//! and the rule that the bearer token never leaves the `Authorization`
//! header. Calls go through the real `curl` to a loopback port. The service
//! is a fake, so none of this is live evidence about a hosted runtime.

#![cfg(unix)]

mod common;
mod http_sim;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{TestResult, backend_id, house, other_house, task_id};
use http_sim::{Fault, SimHttp, descriptor_json};
use kitchen::{
    BackendId, CredentialId, ErrorClass,
    adapters::{
        BackendError, HttpSession, OrcaSession, backend_binding,
        http::{HttpBackend, HttpConfig, HttpError},
        resolve_backend, resolve_http_backend,
    },
    adoption::HouseRegistry,
    contracts::{
        AttemptNumber, BackendUnavailable, BranchName, Capability, CapabilitySet, ContractError,
        CoordinatorMailbox, Effect, EffectExecutor, EffectFailure, EffectRequest, ExternalRef,
        IdempotencyKey, Lookup, MAX_MAILBOX_WAIT, MailMessage, MailboxError, MessageKind,
        NotAppliedReason, Operation, Repository, ResourceKind, ResourceRef, Role, Support, Text,
        UncertainReason, WorkerBackend, WorkerState, Workspace,
        conformance::{self, Check, CheckResult, ConformanceFixture, ConformanceReport},
        fake::FakeBackend,
    },
    house::{BackendBinding, BackendKind, CredentialStatus, HouseConfig, HttpEndpoint},
    scheduling::AgentFamily,
};

const TOKEN: &str = "sandbox-token-7f3a9c";

/// The `curl` on `PATH`; the adapter needs it, so its absence fails.
fn curl() -> TestResult<PathBuf> {
    let path = std::env::var_os("PATH").ok_or("no PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("curl"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "curl is required on PATH for the HTTP backend tests".into())
}

fn config(sim: &SimHttp, coordinator: &str, timeout: Duration) -> TestResult<HttpConfig> {
    Ok(HttpConfig {
        endpoint: HttpEndpoint::new(&sim.endpoint())?,
        backend: backend_id()?,
        house: house()?,
        credential: common::credential()?,
        run: ExternalRef::new("run-1")?,
        coordinator: ExternalRef::new(coordinator)?,
        curl: curl()?,
        call_timeout: timeout,
    })
}

fn connect(sim: &SimHttp) -> TestResult<HttpBackend> {
    Ok(HttpBackend::connect(
        config(sim, "coordinator-1", Duration::from_secs(5))?,
        TOKEN,
    )?)
}

fn capable() -> TestResult<SimHttp> {
    SimHttp::start(FakeBackend::fully_capable(backend_id()?, house()?), TOKEN)
}

fn with(capabilities: &[Capability]) -> TestResult<SimHttp> {
    SimHttp::start(
        FakeBackend::new(
            backend_id()?,
            house()?,
            CapabilitySet::supporting(capabilities.iter().copied()),
        ),
        TOKEN,
    )
}

fn fixture(tag: &str) -> TestResult<ConformanceFixture> {
    Ok(ConformanceFixture {
        house: house()?,
        foreign_house: other_house()?,
        foreign_backend: BackendId::new("fake-other")?,
        credential: common::credential()?,
        repository: Repository::new("origin89hq/km43")?,
        task: task_id("conformance")?,
        run_tag: ExternalRef::new(tag)?,
        brief: Text::new("Conformance probe; exit immediately.")?,
    })
}

fn launch(branch: &str) -> TestResult<Effect> {
    Ok(Effect::Worker(Operation::LaunchWorker {
        role: Role::StationCook,
        workspace: Workspace::Isolated,
        brief: Text::new("Fix the flaky test.")?,
        branch: Some(BranchName::new(branch)?),
        agent: None,
    }))
}

fn request(key: &str, effect: Effect) -> TestResult<EffectRequest> {
    Ok(EffectRequest::new(
        house()?,
        backend_id()?,
        common::credential()?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new(key)?),
        effect,
    ))
}

fn passed(report: &ConformanceReport, checks: &[Check]) {
    for check in checks {
        assert_eq!(report.result(*check), Some(CheckResult::Passed), "{check}");
    }
}

const WORKER_CHECKS: [Check; 15] = [
    Check::DescriptorHouse,
    Check::CrossHouseRefused,
    Check::ForeignBackendRefused,
    Check::UnsupportedRefused,
    Check::UnknownKeyNotApplied,
    Check::ProbeReceipt,
    Check::LookupMatchesReceipt,
    Check::IdempotentResubmission,
    Check::LaunchReceipt,
    Check::SelectionRefused,
    Check::LaunchObservable,
    Check::InventoryListsLaunch,
    Check::MessageRecovery,
    Check::CancelObserved,
    Check::ReleaseKeepsBranch,
];

#[test]
fn a_capable_service_passes_the_executor_suite() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let report = conformance::run(
        &backend,
        &fixture("run-executor")?,
        &launch("kitchen/probe")?,
    )?;
    passed(&report, &WORKER_CHECKS[..8]);
    Ok(())
}

#[test]
fn a_capable_service_passes_the_worker_suite() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let report = conformance::run_worker(&backend, &fixture("run-worker")?)?;
    passed(&report, &WORKER_CHECKS);
    // Lookup, idempotency, and the undeclared selection all reached the
    // service or were refused locally; the probe ran exactly once, plus the
    // message and the cancel and release the suite sends.
    assert_eq!(sim.backend().effects_performed(), 4);
    Ok(())
}

#[test]
fn a_capable_service_passes_the_worker_suite_on_a_chosen_branch() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let branch = BranchName::new("sandbox/run-branch")?;
    let report = conformance::run_worker_on_branch(&backend, &fixture("run-branch")?, &branch)?;
    passed(&report, &WORKER_CHECKS);
    Ok(())
}

fn message(id: &str, kind: MessageKind) -> TestResult<MailMessage> {
    Ok(MailMessage {
        id: ExternalRef::new(id)?,
        kind,
        worker: None,
        outcome: None,
        subject: Some(Text::new("subject")?),
        body: None,
        checkout: kitchen::contracts::CheckoutReport::default(),
    })
}

#[test]
fn a_capable_service_passes_the_mailbox_suite() -> TestResult {
    let sim = capable()?;
    sim.backend()
        .post(vec![message("msg-question", MessageKind::Question)?])?;
    sim.backend().post(vec![
        message("msg-done", MessageKind::WorkerDone)?,
        message("msg-escalation", MessageKind::Escalation)?,
    ])?;
    let sent: Vec<ExternalRef> = ["msg-question", "msg-done", "msg-escalation"]
        .into_iter()
        .map(ExternalRef::new)
        .collect::<Result<_, _>>()?;
    let coordinator = connect(&sim)?;
    let restarted = HttpBackend::connect(
        config(&sim, "coordinator-2", Duration::from_secs(5))?,
        TOKEN,
    )?;
    let report = conformance::run_mailbox(&coordinator, &restarted, &sent)?;
    passed(
        &report,
        &[
            Check::DeliveriesDeclared,
            Check::DeliveryReplayed,
            Check::AdoptionReplays,
            Check::DuplicateAcknowledgement,
            Check::DeliveryOrder,
        ],
    );
    Ok(())
}

#[test]
fn undeclared_effects_are_refused_before_anything_is_sent() -> TestResult {
    let sim = with(&[Capability::WorkerLaunchIsolated])?;
    let backend = connect(&sim)?;
    let report = conformance::run_worker(&backend, &fixture("run-narrow")?)?;
    passed(&report, &[Check::UnsupportedRefused, Check::LaunchReceipt]);
    assert_eq!(
        report.result(Check::CancelObserved),
        Some(CheckResult::NotApplicable {
            requires: Capability::WorkerCancel
        })
    );
    // Only launches reached the service; lookups and the other operations
    // were answered locally.
    let effects: Vec<_> = sim
        .requests()
        .into_iter()
        .filter(|call| call.path != "/v1/descriptor")
        .collect();
    assert!(!effects.is_empty());
    for call in effects {
        assert_eq!(call.path, "/v1/effects");
        assert!(
            call.body.contains(r#""type":"launch-worker""#),
            "{}",
            call.body
        );
    }
    assert_eq!(
        backend.lookup(&request("k", launch("kitchen/k")?)?),
        Err(BackendUnavailable::Unsupported(
            Capability::LookupLaunchWorker
        ))
    );
    assert_eq!(
        backend.inventory(),
        Err(BackendUnavailable::Unsupported(
            Capability::ResourceInventory
        ))
    );
    Ok(())
}

#[test]
fn adoption_needs_the_protocols_answer() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    backend.adopt_run()?;
    sim.inject(Fault::Raw(200, r#"{"status":"fenced"}"#.into()));
    assert_eq!(backend.adopt_run(), Err(MailboxError::Fenced));
    for garbage in [
        r#"{"status":"adopted"}"#,
        r#"{"status":null}"#,
        "<html>proxy</html>",
        "",
    ] {
        sim.inject(Fault::Raw(200, garbage.into()));
        assert_eq!(
            backend.adopt_run(),
            Err(MailboxError::Unavailable(BackendUnavailable::Transport)),
            "{garbage}"
        );
    }
    sim.inject(Fault::Status(503));
    assert_eq!(
        backend.adopt_run(),
        Err(MailboxError::Unavailable(BackendUnavailable::Transport))
    );
    Ok(())
}

#[test]
fn a_request_for_another_credential_is_never_sent() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let fresh = EffectRequest::new(
        house()?,
        backend_id()?,
        CredentialId::new("other-token")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("other-credential")?),
        launch("kitchen/other")?,
    );
    // As the store reads it back after a restart.
    let persisted: EffectRequest = serde_json::from_str(&serde_json::to_string(&fresh)?)?;
    for request in [&fresh, &persisted] {
        assert_eq!(
            backend.execute(request),
            Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
        );
        // Not proof of absence: another executor may hold that credential.
        assert_eq!(backend.lookup(request), Ok(Lookup::Unknown));
    }
    let paths: Vec<_> = sim.requests().into_iter().map(|call| call.path).collect();
    assert_eq!(paths, ["/v1/descriptor"]);
    assert_eq!(sim.backend().effects_performed(), 0);

    // The bound credential still goes through.
    backend.execute(&request("bound-credential", launch("kitchen/bound")?)?)?;
    assert_eq!(sim.backend().effects_performed(), 1);
    Ok(())
}

#[test]
fn redirects_are_not_followed() -> TestResult {
    let sim = capable()?;
    let elsewhere = capable()?;
    sim.inject(Fault::Redirect(format!(
        "{}/v1/descriptor",
        elsewhere.endpoint()
    )));
    let error = HttpBackend::connect(config(&sim, "c", Duration::from_secs(5))?, TOKEN)
        .err()
        .ok_or("a redirected descriptor was accepted")?;
    assert!(matches!(
        error,
        HttpError::Unavailable {
            source: BackendUnavailable::Transport,
            ..
        }
    ));

    let backend = connect(&sim)?;
    let launch = request("redirected", launch("kitchen/redirected")?)?;
    sim.inject(Fault::Redirect(format!(
        "{}/v1/effects",
        elsewhere.endpoint()
    )));
    // Not a typed answer, so the effect is reconciled by lookup.
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    sim.inject(Fault::Redirect(format!(
        "{}/v1/effects/lookup",
        elsewhere.endpoint()
    )));
    assert_eq!(backend.lookup(&launch), Err(BackendUnavailable::Transport));
    // Neither the token nor any request reached the redirect target.
    assert!(elsewhere.requests().is_empty());
    assert_eq!(sim.backend().effects_performed(), 0);
    Ok(())
}

#[test]
fn the_token_travels_only_in_the_authorization_header() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    conformance::run_worker(&backend, &fixture("run-token")?)?;
    let requests = sim.requests();
    assert!(requests.len() > 10);
    for call in &requests {
        assert_eq!(
            call.authorization.as_deref(),
            Some(&*format!("Bearer {TOKEN}"))
        );
        assert!(!call.body.contains(TOKEN), "{}", call.body);
        if call.path == "/v1/effects" {
            // The credential is named, never supplied.
            assert!(call.body.contains(r#""credential":"origin89-orca""#));
            let body: serde_json::Value = serde_json::from_str(&call.body)?;
            assert_eq!(
                call.idempotency_key.as_deref(),
                body["request"]["key"].as_str()
            );
        }
    }
    Ok(())
}

#[test]
fn the_token_never_reaches_curl_arguments() -> TestResult {
    let temp = tempfile::tempdir()?;
    let args = temp.path().join("args");
    let fake_curl = temp.path().join("curl");
    common::executable::write_executable(
        &fake_curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 7\n",
            args.display()
        ),
    )?;
    let sim = capable()?;
    let error = HttpBackend::connect(
        HttpConfig {
            curl: fake_curl,
            ..config(&sim, "coordinator-1", Duration::from_secs(5))?
        },
        TOKEN,
    )
    .err()
    .ok_or("the fake curl cannot connect")?;
    assert!(matches!(
        error,
        HttpError::Unavailable {
            source: BackendUnavailable::Transport,
            ..
        }
    ));
    assert_eq!(fs::read_to_string(&args)?, "-q\n--config\n-\n");
    Ok(())
}

#[test]
fn a_lost_response_is_uncertain_and_reconciles_by_lookup() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let launch = request("lost", launch("kitchen/lost")?)?;
    sim.inject(Fault::GarbageAfterActing);
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::ResponseLost))
    );
    let Lookup::Applied(receipt) = backend.lookup(&launch)? else {
        return Err("the applied launch was not found".into());
    };
    // Resubmitting the key returns the original receipt without a second launch.
    assert_eq!(backend.execute(&launch)?, receipt);
    assert_eq!(sim.backend().effects_performed(), 1);

    let dropped = request("dropped", self::launch("kitchen/dropped")?)?;
    sim.inject(Fault::DropAfterActing);
    assert_eq!(
        backend.execute(&dropped),
        Err(EffectFailure::Uncertain(UncertainReason::Transport))
    );
    assert!(matches!(backend.lookup(&dropped)?, Lookup::Applied(_)));
    Ok(())
}

#[test]
fn a_deadline_is_uncertain_not_refused() -> TestResult {
    let sim = capable()?;
    let hasty = HttpBackend::connect(
        config(&sim, "coordinator-1", Duration::from_secs(1))?,
        TOKEN,
    )?;
    let launch = request("slow", launch("kitchen/slow")?)?;
    sim.inject(Fault::Delay(Duration::from_millis(2500)));
    let started = Instant::now();
    assert_eq!(
        hasty.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::Timeout))
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    // The service acted after the client gave up; a patient lookup sees it.
    let patient = connect(&sim)?;
    assert!(matches!(patient.lookup(&launch)?, Lookup::Applied(_)));
    Ok(())
}

#[test]
fn refusals_before_acting_are_typed_not_applied() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let cases = [
        (
            Fault::Status(401),
            EffectFailure::NotApplied(NotAppliedReason::Rejected),
        ),
        (
            Fault::Status(429),
            EffectFailure::NotApplied(NotAppliedReason::RateLimited { retry_after: None }),
        ),
        (
            Fault::Raw(
                200,
                r#"{"status":"not-applied","reason":"rate-limited","retryAfterSeconds":30}"#
                    .into(),
            ),
            EffectFailure::NotApplied(NotAppliedReason::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            }),
        ),
        (
            Fault::Raw(
                200,
                r#"{"status":"not-applied","reason":"unsupported","capability":"worker.messaging"}"#
                    .into(),
            ),
            EffectFailure::NotApplied(NotAppliedReason::Unsupported(Capability::WorkerMessaging)),
        ),
        // Not a typed answer: the service may have acted.
        (
            Fault::Status(500),
            EffectFailure::Uncertain(UncertainReason::ResponseLost),
        ),
        (
            Fault::Raw(200, r#"{"status":"maybe"}"#.into()),
            EffectFailure::Uncertain(UncertainReason::ResponseLost),
        ),
        // An applied receipt naming another backend's worker is not trusted.
        (
            Fault::Raw(
                200,
                r#"{"status":"applied","receipt":{"reference":"r","created":[{"kind":"worker","backend":"elsewhere","handle":"w"}],"touched":[]}}"#
                    .into(),
            ),
            EffectFailure::Uncertain(UncertainReason::ResponseLost),
        ),
    ];
    for (n, (fault, expected)) in cases.into_iter().enumerate() {
        sim.inject(fault);
        let launch = request(&format!("refused-{n}"), launch(&format!("kitchen/r{n}"))?)?;
        assert_eq!(backend.execute(&launch), Err(expected), "case {n}");
    }
    // The faults answered before the fake acted.
    assert_eq!(sim.backend().effects_performed(), 0);
    Ok(())
}

#[test]
fn an_unreachable_service_is_uncertain_for_effects_and_unavailable_for_reads() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    drop(sim);
    let launch = request("unreachable", launch("kitchen/unreachable")?)?;
    assert_eq!(
        backend.execute(&launch),
        Err(EffectFailure::Uncertain(UncertainReason::Transport))
    );
    assert_eq!(backend.lookup(&launch), Err(BackendUnavailable::Transport));
    assert_eq!(backend.inventory(), Err(BackendUnavailable::Transport));
    Ok(())
}

#[test]
fn connecting_checks_the_token_and_the_services_identity() -> TestResult {
    let sim = capable()?;
    assert!(matches!(
        HttpBackend::connect(config(&sim, "c", Duration::from_secs(5))?, "wrong-token"),
        Err(HttpError::Unauthorized { .. })
    ));
    for invalid in ["", "two words", "line\nbreak"] {
        assert_eq!(
            HttpBackend::connect(config(&sim, "c", Duration::from_secs(5))?, invalid).err(),
            Some(HttpError::InvalidConfig)
        );
    }
    for timeout in [Duration::ZERO, Duration::from_secs(26)] {
        assert_eq!(
            HttpBackend::connect(config(&sim, "c", timeout)?, TOKEN).err(),
            Some(HttpError::InvalidConfig)
        );
    }
    let relative = HttpConfig {
        curl: PathBuf::from("curl"),
        ..config(&sim, "c", Duration::from_secs(5))?
    };
    assert_eq!(
        HttpBackend::connect(relative, TOKEN).err(),
        Some(HttpError::InvalidConfig)
    );
    // Nothing above but the wrong token reached the service.
    assert_eq!(sim.requests().len(), 1);

    let foreign = SimHttp::start(
        FakeBackend::fully_capable(backend_id()?, other_house()?),
        TOKEN,
    )?;
    let error = HttpBackend::connect(config(&foreign, "c", Duration::from_secs(5))?, TOKEN)
        .err()
        .ok_or("a service for another house was accepted")?;
    assert!(matches!(error, HttpError::DescriptorMismatch { .. }));
    assert_eq!(error.class(), ErrorClass::Refused);

    let garbled = SimHttp::start_with(
        FakeBackend::fully_capable(backend_id()?, house()?),
        TOKEN,
        serde_json::json!({ "backend": "fake" }),
    )?;
    assert!(matches!(
        HttpBackend::connect(config(&garbled, "c", Duration::from_secs(5))?, TOKEN),
        Err(HttpError::Malformed { .. })
    ));
    Ok(())
}

#[test]
fn observation_is_typed_and_never_guessed() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request("observed", launch("kitchen/observed")?)?)?;
    let worker = receipt
        .created()
        .iter()
        .find(|resource| resource.kind == ResourceKind::Worker)
        .ok_or("no worker")?
        .clone();
    assert_eq!(backend.observe_worker(&worker)?, WorkerState::Starting);
    sim.backend()
        .set_worker_state(&worker, WorkerState::AwaitingReply);
    assert_eq!(backend.observe_worker(&worker)?, WorkerState::AwaitingReply);
    sim.inject(Fault::Raw(200, r#"{"state":"hibernating"}"#.into()));
    assert_eq!(backend.observe_worker(&worker)?, WorkerState::Unknown);
    sim.inject(Fault::Status(503));
    assert_eq!(
        backend.observe_worker(&worker),
        Err(BackendUnavailable::Transport)
    );
    let calls = sim.requests().len();
    let foreign = ResourceRef {
        backend: BackendId::new("elsewhere")?,
        ..worker
    };
    assert_eq!(backend.observe_worker(&foreign)?, WorkerState::Missing);
    assert_eq!(
        sim.requests().len(),
        calls,
        "a foreign worker is not asked about"
    );
    Ok(())
}

#[test]
fn usage_is_read_when_declared() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    let receipt = backend.execute(&request("usage", launch("kitchen/usage")?)?)?;
    let worker = receipt.created().first().ok_or("no worker")?.clone();
    assert_eq!(backend.worker_usage(&worker)?, None);
    sim.set_usage(
        worker.handle.as_str(),
        serde_json::json!({
            "source": "run-42",
            "agent": "codex",
            "tokens": { "input": 1200, "output": 300 },
            "cost": { "amount": 51000, "basis": "reported" },
        }),
    );
    let report = backend.worker_usage(&worker)?.ok_or("no report")?;
    assert_eq!(report.source.as_str(), "run-42");
    assert_eq!(report.agent, Some(AgentFamily::Codex));
    assert_eq!(report.tokens.input, Some(1200));
    assert_eq!(report.tokens.cache_write, None);
    sim.inject(Fault::Raw(200, r#"{"status":"reported"}"#.into()));
    assert_eq!(
        backend.worker_usage(&worker),
        Err(BackendUnavailable::Transport)
    );

    let silent = with(&[Capability::WorkerLaunchIsolated])?;
    assert_eq!(
        connect(&silent)?.worker_usage(&worker),
        Err(BackendUnavailable::Unsupported(
            Capability::UsageAttribution
        ))
    );
    Ok(())
}

#[test]
fn a_wait_is_bounded_even_when_the_service_answers_at_once() -> TestResult {
    let sim = capable()?;
    let backend = connect(&sim)?;
    assert_eq!(backend.await_delivery(Duration::from_millis(200))?, None);
    assert_eq!(sim.await_calls(), 1);
    let started = Instant::now();
    assert_eq!(backend.await_delivery(MAX_MAILBOX_WAIT)?, None);
    let calls = sim.await_calls() - 1;
    assert!((2..=32).contains(&calls), "{calls} calls");
    assert!(started.elapsed() < Duration::from_secs(60));
    // A waiting batch ends the wait.
    sim.backend()
        .post(vec![message("msg-q", MessageKind::Question)?])?;
    let batch = backend
        .await_delivery(MAX_MAILBOX_WAIT)?
        .ok_or("no batch")?;
    assert_eq!(batch.messages.len(), 1);
    Ok(())
}

// --- The resolver ------------------------------------------------------

fn http_house(endpoint: Option<&str>) -> TestResult<HouseConfig> {
    let mut config: HouseConfig =
        serde_json::from_str(include_str!("fixtures/house/origin89.json"))?;
    config.backend = Some(BackendBinding {
        kind: BackendKind::Http.into(),
        backend: backend_id()?,
        credential: CredentialId::new("sandbox-token")?,
        endpoint: endpoint.map(HttpEndpoint::new).transpose()?,
    });
    Ok(config)
}

/// A registry holding `house`, with its token file written at `mode`.
fn registry_with(
    root: &Path,
    house: &HouseConfig,
    token: Option<(&str, u32)>,
) -> TestResult<HouseRegistry> {
    // The registry refuses a path through a link, such as macOS's `/var`.
    let root = root.canonicalize()?;
    let registry = HouseRegistry::new(root.join("registry"))?;
    registry.initialize(house)?;
    if let Some((token, mode)) = token {
        let credentials = registry.private_path(&house.house)?.join("credentials");
        fs::create_dir_all(&credentials)?;
        let file = credentials.join("sandbox-token");
        fs::write(&file, token)?;
        fs::set_permissions(&file, fs::Permissions::from_mode(mode))?;
    }
    Ok(registry)
}

fn session() -> TestResult<HttpSession> {
    Ok(HttpSession {
        run: ExternalRef::new("run-1")?,
        coordinator: ExternalRef::new("coordinator-1")?,
        curl: curl()?,
        call_timeout: Duration::from_secs(5),
    })
}

#[test]
fn the_resolver_builds_the_bound_http_backend() -> TestResult {
    let sim = capable()?;
    let temp = tempfile::tempdir()?;
    let house = http_house(Some(&sim.endpoint()))?;
    let registry = registry_with(temp.path(), &house, Some((&format!("{TOKEN}\n"), 0o600)))?;
    let backend = resolve_http_backend(
        &registry,
        &house,
        session()?,
        &[
            Capability::WorkerLaunchIsolated,
            Capability::WorkerDeliveries,
            Capability::RunTransfer,
        ],
    )?;
    assert_eq!(backend.descriptor().house, house.house);
    let bound = EffectRequest::new(
        house.house.clone(),
        backend_id()?,
        CredentialId::new("sandbox-token")?,
        task_id("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("resolved")?),
        launch("kitchen/resolved")?,
    );
    assert!(backend.execute(&bound).is_ok());
    // A request authorized under another credential is refused unsent.
    assert_eq!(
        backend.execute(&request("unbound", launch("kitchen/unbound")?)?),
        Err(EffectFailure::NotApplied(NotAppliedReason::Rejected))
    );
    assert_eq!(sim.backend().effects_performed(), 1);
    Ok(())
}

#[test]
fn declared_capabilities_are_what_the_service_reports_within_the_protocol() -> TestResult {
    // The service reports everything, messaging only partly, and no
    // deliveries or run transfer.
    let fake = FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting(Capability::ALL.into_iter().filter(|capability| {
            !matches!(
                capability,
                Capability::WorkerDeliveries | Capability::RunTransfer
            )
        }))
        .with(Capability::WorkerMessaging, Support::Partial),
    );
    let sim = SimHttp::start_with(
        FakeBackend::fully_capable(backend_id()?, house()?),
        TOKEN,
        descriptor_json(&fake),
    )?;
    let backend = connect(&sim)?;
    let declared = &backend.descriptor().capabilities;
    assert!(declared.supports(Capability::WorkerLaunchIsolated));
    assert_eq!(
        declared.support(Capability::WorkerMessaging),
        Some(Support::Partial)
    );
    // Reported, but the protocol has no calls for them.
    for beyond in [
        Capability::ScheduleManage,
        Capability::ForgeMutation,
        Capability::AskHuman,
        Capability::SessionReuse,
    ] {
        assert_eq!(declared.support(beyond), None, "{beyond}");
    }

    let temp = tempfile::tempdir()?;
    let house = http_house(Some(&sim.endpoint()))?;
    let registry = registry_with(temp.path(), &house, Some((TOKEN, 0o600)))?;
    let error = resolve_http_backend(
        &registry,
        &house,
        session()?,
        &[
            Capability::WorkerDeliveries,
            Capability::WorkerMessaging,
            Capability::ScheduleManage,
            Capability::WorkerLaunchIsolated,
        ],
    )
    .err()
    .ok_or("a backend without required capabilities was built")?;
    let BackendError::Unsupported { kind, source, .. } = &error else {
        return Err(format!("unexpected {error}").into());
    };
    assert_eq!(*kind, BackendKind::Http);
    assert_eq!(
        *source,
        ContractError::UnsupportedCapabilities {
            missing: vec![Capability::ScheduleManage, Capability::WorkerDeliveries],
            partial: vec![Capability::WorkerMessaging],
        }
    );
    let text = error.to_string();
    assert!(
        text.contains("schedule.manage") && text.contains("worker.deliveries"),
        "{text}"
    );
    Ok(())
}

#[test]
fn the_resolver_refuses_bad_bindings_and_tokens_before_contacting_the_service() -> TestResult {
    let sim = capable()?;
    let endpoint = sim.endpoint();
    let temp = tempfile::tempdir()?;

    let unbound = http_house(None)?;
    let registry = registry_with(temp.path(), &unbound, Some((TOKEN, 0o600)))?;
    assert!(matches!(
        resolve_http_backend(&registry, &unbound, session()?, &[]),
        Err(BackendError::Endpoint {
            kind: BackendKind::Http,
            ..
        })
    ));

    let house = http_house(Some(&endpoint))?;
    for (name, token, expected) in [
        ("missing", None, Some(CredentialStatus::Missing)),
        (
            "exposed",
            Some((TOKEN, 0o644)),
            Some(CredentialStatus::Exposed),
        ),
        (
            "oversized",
            Some((&*"a".repeat(16 * 1024 + 1), 0o600)),
            None,
        ),
    ] {
        let root = temp.path().join(name);
        fs::create_dir(&root)?;
        let registry = registry_with(&root, &house, token)?;
        let error = resolve_http_backend(&registry, &house, session()?, &[])
            .err()
            .ok_or("an unusable token was accepted")?;
        match expected {
            Some(status) => assert!(
                matches!(&error, BackendError::CredentialUnavailable { status: found, .. } if *found == status),
                "{name}: {error}"
            ),
            None => assert!(
                matches!(error, BackendError::Http(HttpError::InvalidConfig)),
                "{name}: {error}"
            ),
        }
    }
    assert!(sim.requests().is_empty());

    // An Orca binding is not an HTTP backend, and the reverse.
    let mut orca = http_house(None)?;
    if let Some(binding) = orca.backend.as_mut() {
        binding.kind = BackendKind::Orca.into();
    }
    let root = temp.path().join("orca");
    fs::create_dir(&root)?;
    let registry = registry_with(&root, &orca, None)?;
    assert!(matches!(
        resolve_http_backend(&registry, &orca, session()?, &[]),
        Err(BackendError::KindMismatch {
            bound: BackendKind::Orca,
            needed: BackendKind::Http,
            ..
        })
    ));
    let orca_session = OrcaSession {
        run: ExternalRef::new("run")?,
        coordinator: ExternalRef::new("term")?,
        repo: ExternalRef::new("repo")?,
        base_branch: None,
        branch_prefix: None,
        agent: AgentFamily::Claude,
        call_timeout: Duration::from_secs(1),
        launch_timeout: Duration::from_secs(1),
        runtime_dir: temp.path().join("runtime"),
        reservation_timeout: Duration::from_secs(1),
    };
    let error = resolve_backend(
        &house,
        orca_session,
        kitchen::adapters::orca::SystemRunner::new(Path::new("/nonexistent/orca")),
        &[],
    )
    .err()
    .ok_or("an HTTP binding built Orca")?;
    assert!(matches!(
        error,
        BackendError::KindMismatch {
            bound: BackendKind::Http,
            needed: BackendKind::Orca,
            ..
        }
    ));
    // An endpoint on an Orca binding is refused too.
    let mut orca_with_endpoint = house.clone();
    if let Some(binding) = orca_with_endpoint.backend.as_mut() {
        binding.kind = BackendKind::Orca.into();
    }
    assert!(matches!(
        backend_binding(&orca_with_endpoint),
        Err(BackendError::Endpoint {
            kind: BackendKind::Orca,
            ..
        })
    ));
    Ok(())
}

/// Commands build HTTP backends only through `resolve_http_backend`.
#[test]
fn only_the_resolver_connects_to_an_http_backend() -> TestResult {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no crates directory")?;
    let mut callers = Vec::new();
    for entry in fs::read_dir(crates)? {
        let source = entry?.path().join("src");
        if source.is_dir() {
            visit(&source, &mut |path, text| {
                if text.contains("HttpBackend::connect(") {
                    callers.push(path.to_path_buf());
                }
            })?;
        }
    }
    assert_eq!(
        callers,
        vec![crates.join("kitchen/src/adapters/resolve.rs")]
    );
    Ok(())
}

fn visit(dir: &Path, found: &mut dyn FnMut(&Path, &str)) -> TestResult {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit(&path, found)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found(&path, &fs::read_to_string(&path)?);
        }
    }
    Ok(())
}

#[test]
fn a_url_pattern_in_the_endpoint_sends_one_request() -> TestResult {
    let sim = capable()?;
    let config = HttpConfig {
        endpoint: HttpEndpoint::new(&format!("{}/x{{a,b}}", sim.endpoint()))?,
        ..config(&sim, "coordinator-1", Duration::from_secs(5))?
    };
    // Connecting may fail against this path; what matters is that curl sent
    // exactly one request and did not expand the braces into two URLs.
    let _ = HttpBackend::connect(config, TOKEN);
    let paths: Vec<String> = sim.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(
        !paths.iter().any(|path| path.starts_with("/xa")),
        "{paths:?}"
    );
    Ok(())
}
