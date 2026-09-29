//! The coordinator mailbox contract: at-least-once delivery, acknowledgement,
//! and run adoption after a restart, checked by `conformance::run_mailbox`
//! against the in-memory fake and against fakes that break one rule each.
//! These are simulated results, not live runtime evidence; the Orca adapter
//! runs the same checks on its simulated runtime in `orca_adapter.rs`.

mod common;

use std::{fs, path::Path, time::Duration};

use common::{TestResult, backend_id, house};
use kitchen::contracts::{
    BackendDescriptor, BackendUnavailable, Capability, CapabilitySet, CoordinatorMailbox, Delivery,
    EffectExecutor, EffectFailure, EffectRequest, ExternalRef, Lookup, MailMessage, MailboxError,
    MessageKind, Receipt, ResourceRef, WorkerBackend, WorkerState,
    conformance::{self, Check, CheckResult, ConformanceFailure},
    fake::FakeBackend,
};

fn message(id: &str, kind: MessageKind) -> TestResult<MailMessage> {
    Ok(MailMessage {
        id: ExternalRef::new(id)?,
        kind,
        worker: None,
        outcome: None,
        subject: None,
        body: None,
    })
}

/// Two batches holding a question, then a report and an escalation.
fn seed(fake: &FakeBackend) -> TestResult<Vec<ExternalRef>> {
    fake.post(vec![message("msg-question", MessageKind::Question)?])?;
    fake.post(vec![
        message("msg-done", MessageKind::WorkerDone)?,
        message("msg-escalation", MessageKind::Escalation)?,
    ])?;
    Ok(["msg-question", "msg-done", "msg-escalation"]
        .into_iter()
        .map(ExternalRef::new)
        .collect::<Result<_, _>>()?)
}

fn without(capabilities: &[Capability]) -> TestResult<FakeBackend> {
    let declared = Capability::ALL
        .into_iter()
        .filter(|capability| !capabilities.contains(capability));
    Ok(FakeBackend::new(
        backend_id()?,
        house()?,
        CapabilitySet::supporting(declared),
    ))
}

fn passed(report: &conformance::ConformanceReport, checks: &[Check]) {
    for check in checks {
        assert_eq!(report.result(*check), Some(CheckResult::Passed), "{check}");
    }
}

#[test]
fn the_fake_mailbox_conforms() -> TestResult {
    let fake = FakeBackend::fully_capable(backend_id()?, house()?);
    let sent = seed(&fake)?;
    let report = conformance::run_mailbox(&fake, &fake.restarted(), &sent)?;
    assert_eq!(
        report.result(Check::DeliveriesUndeclared),
        Some(CheckResult::NothingUndeclared)
    );
    passed(
        &report,
        &[
            Check::DeliveryReplayed,
            Check::AdoptionReplays,
            Check::DuplicateAcknowledgement,
            Check::DeliveryOrder,
        ],
    );
    Ok(())
}

#[test]
fn a_crash_before_acknowledging_loses_nothing() -> TestResult {
    let fake = FakeBackend::fully_capable(backend_id()?, house()?);
    seed(&fake)?;
    let read = fake.next_delivery()?.ok_or("a batch")?;
    // The coordinator stops here, before handling or acknowledging.
    let adopter = fake.restarted();
    adopter.adopt_run()?;
    assert_eq!(adopter.next_delivery()?, Some(read.clone()));
    assert_eq!(
        adopter.await_delivery(Duration::from_secs(1))?,
        Some(read.clone())
    );
    assert_eq!(fake.next_delivery(), Err(MailboxError::Fenced));
    assert_eq!(fake.acknowledge(&read.id), Err(MailboxError::Fenced));
    // The fenced coordinator's acknowledgement consumed nothing.
    assert_eq!(adopter.next_delivery()?, Some(read));
    Ok(())
}

#[test]
fn an_empty_mailbox_is_a_checkpoint() -> TestResult {
    let fake = FakeBackend::fully_capable(backend_id()?, house()?);
    assert_eq!(fake.next_delivery()?, None);
    assert_eq!(fake.await_delivery(Duration::from_secs(1))?, None);
    // Acknowledging a batch that is not there changes nothing.
    assert_eq!(fake.acknowledge(&ExternalRef::new("delivery-gone")?)?, None);
    let sent = seed(&fake)?;
    let first = fake.next_delivery()?.ok_or("a batch")?;
    assert_eq!(
        fake.acknowledge(&ExternalRef::new("delivery-gone")?)?,
        Some(first.clone()),
        "an unknown id consumes no batch"
    );
    assert_eq!(
        first.messages.first().map(|m| m.id.clone()),
        sent.first().cloned()
    );
    Ok(())
}

#[test]
fn undeclared_deliveries_and_adoption_are_refused() -> TestResult {
    let silent = without(&[Capability::WorkerDeliveries])?;
    assert_eq!(
        silent.next_delivery(),
        Err(MailboxError::Unavailable(BackendUnavailable::Unsupported(
            Capability::WorkerDeliveries
        )))
    );
    let sent = seed(&silent)?;
    let report = conformance::run_mailbox(&silent, &silent.restarted(), &sent)?;
    assert_eq!(
        report.result(Check::DeliveriesUndeclared),
        Some(CheckResult::Passed)
    );
    assert_eq!(
        report.result(Check::DeliveryOrder),
        Some(CheckResult::NotApplicable {
            requires: Capability::WorkerDeliveries
        })
    );

    // Deliveries without run transfer: one coordinator drains the mailbox.
    let fixed = without(&[Capability::RunTransfer])?;
    let sent = seed(&fixed)?;
    let restarted = fixed.restarted();
    assert_eq!(
        restarted.adopt_run(),
        Err(MailboxError::Unavailable(BackendUnavailable::Unsupported(
            Capability::RunTransfer
        )))
    );
    let report = conformance::run_mailbox(&fixed, &restarted, &sent)?;
    assert_eq!(
        report.result(Check::AdoptionReplays),
        Some(CheckResult::NotApplicable {
            requires: Capability::RunTransfer
        })
    );
    passed(
        &report,
        &[Check::DuplicateAcknowledgement, Check::DeliveryOrder],
    );
    Ok(())
}

#[test]
fn a_fixture_without_messages_is_refused() -> TestResult {
    let fake = FakeBackend::fully_capable(backend_id()?, house()?);
    let failure = conformance::run_mailbox(&fake, &fake.restarted(), &[])
        .err()
        .ok_or("an empty fixture passed")?;
    assert_eq!(failure.check, Check::Fixture);
    Ok(())
}

/// One broken mailbox rule.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Reading consumes the batch: no replay.
    ConsumeOnRead,
    /// Any acknowledgement consumes the oldest batch, whatever it names.
    AckConsumesOldest,
    /// Adoption succeeds without fencing the previous coordinator.
    NoFence,
    /// Messages within a batch come out reversed.
    Reorder,
}

/// The fake with one mailbox rule broken; everything else delegates.
struct Broken {
    inner: FakeBackend,
    fault: Fault,
}

impl EffectExecutor for Broken {
    fn descriptor(&self) -> &BackendDescriptor {
        self.inner.descriptor()
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        self.inner.execute(request)
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        self.inner.lookup(request)
    }
}

impl WorkerBackend for Broken {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.inner.observe_worker(worker)
    }
}

impl Broken {
    fn shape(&self, delivery: Option<Delivery>) -> Option<Delivery> {
        delivery.map(|mut delivery| {
            if self.fault == Fault::Reorder {
                delivery.messages.reverse();
            }
            delivery
        })
    }
}

impl CoordinatorMailbox for Broken {
    fn adopt_run(&self) -> Result<(), MailboxError> {
        if self.fault == Fault::NoFence {
            return Ok(());
        }
        self.inner.adopt_run()
    }

    fn next_delivery(&self) -> Result<Option<Delivery>, MailboxError> {
        let delivery = self.inner.next_delivery()?;
        if self.fault == Fault::ConsumeOnRead
            && let Some(delivery) = &delivery
        {
            self.inner.acknowledge(&delivery.id)?;
        }
        Ok(self.shape(delivery))
    }

    fn acknowledge(&self, delivery: &ExternalRef) -> Result<Option<Delivery>, MailboxError> {
        let target = match (self.fault, self.inner.next_delivery()?) {
            (Fault::AckConsumesOldest, Some(oldest)) => oldest.id,
            _ => delivery.clone(),
        };
        let next = self.inner.acknowledge(&target)?;
        Ok(self.shape(next))
    }

    fn await_delivery(&self, wait: Duration) -> Result<Option<Delivery>, MailboxError> {
        let delivery = self.inner.await_delivery(wait)?;
        Ok(self.shape(delivery))
    }
}

fn broken_run(fault: Fault) -> TestResult<ConformanceFailure> {
    let fake = FakeBackend::fully_capable(backend_id()?, house()?);
    let sent = seed(&fake)?;
    let coordinator = Broken {
        inner: fake.restarted(),
        fault,
    };
    let restarted = Broken {
        inner: fake.restarted(),
        fault,
    };
    conformance::run_mailbox(&coordinator, &restarted, &sent)
        .err()
        .ok_or_else(|| "a broken mailbox passed".into())
}

#[test]
fn each_broken_rule_fails_its_check() -> TestResult {
    for (fault, check) in [
        (Fault::ConsumeOnRead, Check::DeliveryReplayed),
        (Fault::NoFence, Check::AdoptionReplays),
        (Fault::AckConsumesOldest, Check::DuplicateAcknowledgement),
        (Fault::Reorder, Check::DeliveryOrder),
    ] {
        assert_eq!(broken_run(fault)?.check, check);
    }
    Ok(())
}

/// Coordination and commands reach backends only through the contract: no
/// crate source outside `adapters/` names the Orca backend type, so no
/// Orca-only method can be called there. Tests may use it directly.
#[test]
fn only_adapters_name_the_orca_backend() -> TestResult {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no crates directory")?;
    let adapters = crates.join("kitchen/src/adapters");
    let mut naming = Vec::new();
    for entry in fs::read_dir(crates)? {
        let source = entry?.path().join("src");
        if source.is_dir() {
            visit(&source, &mut |path, text| {
                if !path.starts_with(&adapters) && text.contains("OrcaBackend") {
                    naming.push(path.to_path_buf());
                }
            })?;
        }
    }
    assert_eq!(naming, Vec::<std::path::PathBuf>::new());
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
