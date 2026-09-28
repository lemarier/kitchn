//! Durable workflow markers: facts keyed by workflow, work item, and exact
//! evidence subject. Simulated with temporary stores.

mod common;

use std::{fs, num::NonZeroU64};

use common::{Fixture, TestResult, at, commit, scheduled, ttl};
use kitchen::{
    ConsumerId, Error, WorkflowId,
    contracts::{EvidenceSubject, EvidenceVerdict, ExternalRef, Repository},
    state::{
        Corruption, MAX_MARKERS, MarkerFact, MarkerKey, MarkerRecording, StateError, WorkItem,
    },
};

fn pull_request(number: u64) -> TestResult<WorkItem> {
    Ok(WorkItem::PullRequest {
        repository: Repository::new("origin89hq/km43")?,
        number: NonZeroU64::new(number).ok_or("zero")?,
    })
}

fn key(workflow: &str, item: WorkItem, head: char, base: Option<char>) -> TestResult<MarkerKey> {
    Ok(MarkerKey {
        workflow: WorkflowId::new(workflow)?,
        item,
        subject: EvidenceSubject {
            head: commit(head)?,
            base: base.map(commit).transpose()?,
        },
    })
}

type Expectation = fn(&Error) -> bool;

fn verdict(verdict: EvidenceVerdict) -> MarkerFact {
    MarkerFact::Verdict { verdict }
}

#[test]
fn recording_the_same_fact_again_changes_nothing() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', Some('b'))?;
    let first = fixture.store.record_marker(
        gate.clone(),
        verdict(EvidenceVerdict::Pass),
        &scheduled("gate-tick-1")?,
        at(1),
    )?;
    let MarkerRecording::Recorded(recorded) = first else {
        return Err("expected a new marker".into());
    };
    assert_eq!(recorded.recorded_at(), at(1));
    assert_eq!(
        fixture.store.record_marker(
            gate.clone(),
            verdict(EvidenceVerdict::Pass),
            &scheduled("gate-tick-2")?,
            at(2)
        )?,
        MarkerRecording::AlreadyRecorded(recorded.clone()),
        "the original marker, time, and recorder are kept"
    );
    // A different fact for the same head is refused, not overwritten.
    assert!(matches!(
        fixture.store.record_marker(
            gate.clone(),
            verdict(EvidenceVerdict::Fail),
            &scheduled("gate-tick-3")?,
            at(3)
        ),
        Err(Error::State(StateError::MarkerConflict))
    ));
    assert_eq!(fixture.store.marker(&gate)?, Some(recorded));
    Ok(())
}

#[test]
fn a_moved_head_or_base_or_another_item_is_a_distinct_key() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    let keys = [
        key("merge-gate", pull_request(20)?, 'a', Some('b'))?,
        key("merge-gate", pull_request(20)?, 'c', Some('b'))?,
        key("merge-gate", pull_request(20)?, 'a', Some('d'))?,
        key("merge-gate", pull_request(20)?, 'a', None)?,
        key("merge-gate", pull_request(21)?, 'a', Some('b'))?,
        key(
            "merge-gate",
            WorkItem::Issue {
                repository: Repository::new("origin89hq/km43")?,
                number: NonZeroU64::new(20).ok_or("zero")?,
            },
            'a',
            Some('b'),
        )?,
        key("triage", pull_request(20)?, 'a', Some('b'))?,
    ];
    for key in &keys {
        assert!(matches!(
            fixture.store.record_marker(
                key.clone(),
                verdict(EvidenceVerdict::Pass),
                &recorder,
                at(1)
            )?,
            MarkerRecording::Recorded(_)
        ));
    }
    assert_eq!(
        fixture
            .store
            .markers(&WorkflowId::new("merge-gate")?)?
            .len(),
        6
    );
    assert_eq!(fixture.store.markers(&WorkflowId::new("triage")?)?.len(), 1);
    assert_eq!(
        fixture
            .store
            .marker(&key("merge-gate", pull_request(99)?, 'a', None)?)?,
        None
    );
    Ok(())
}

#[test]
fn markers_persist_and_perform_no_effects() -> TestResult {
    let fixture = Fixture::new()?;
    let question = key("triage", pull_request(7)?, 'a', None)?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-17")?,
    };
    fixture.store.record_marker(
        question.clone(),
        asked.clone(),
        &scheduled("triage-tick")?,
        at(1),
    )?;
    let reopened = fixture.reopen()?;
    let marker = reopened.marker(&question)?.ok_or("marker lost on reopen")?;
    assert_eq!(marker.fact(), &asked);
    assert_eq!(marker.recorded_by(), &scheduled("triage-tick")?);
    assert!(
        reopened.tasks()?.is_empty(),
        "a marker is not a task or an effect"
    );
    Ok(())
}

#[test]
fn a_superseded_consumer_cannot_record_markers() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let gate = ConsumerId::new("gate-origin89")?;
    let first = store.acquire_consumer(&gate, &scheduled("tick-1")?, ttl(60)?, at(0))?;
    let tick1 = scheduled("tick-1")?.under(gate.clone(), first.fence());
    store.record_marker(
        key("merge-gate", pull_request(1)?, 'a', None)?,
        verdict(EvidenceVerdict::Pass),
        &tick1,
        at(1),
    )?;
    store.take_over_consumer(&gate, &scheduled("tick-2")?, ttl(60)?, at(61))?;
    assert!(matches!(
        store.record_marker(
            key("merge-gate", pull_request(2)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &tick1,
            at(62)
        ),
        Err(Error::State(StateError::StaleFence { .. }))
    ));
    assert_eq!(store.markers(&WorkflowId::new("merge-gate")?)?.len(), 1);
    Ok(())
}

#[test]
fn the_marker_count_is_bounded() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    fixture.store.record_marker(
        key("merge-gate", pull_request(1)?, 'a', None)?,
        verdict(EvidenceVerdict::Pass),
        &recorder,
        at(1),
    )?;
    // Fill the store to its bound by copying the one valid marker.
    let path = fixture.state_path();
    let mut state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let template = state["markers"][0].clone();
    let markers: Vec<_> = (1..=MAX_MARKERS)
        .map(|number| {
            let mut marker = template.clone();
            marker["key"]["item"]["number"] = number.into();
            marker
        })
        .collect();
    state["markers"] = markers.into();
    fs::write(&path, serde_json::to_vec(&state)?)?;
    let store = fixture.reopen()?;

    let extra = key("merge-gate", pull_request(99_999)?, 'a', None)?;
    assert!(matches!(
        store.record_marker(
            extra.clone(),
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(2)
        ),
        Err(Error::State(StateError::CapacityExceeded {
            limit: kitchen::state::Limit::Markers
        }))
    ));
    assert_eq!(store.marker(&extra)?, None);
    // Repeating an existing fact still succeeds when full.
    assert!(matches!(
        store.record_marker(
            key("merge-gate", pull_request(1)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(2)
        )?,
        MarkerRecording::AlreadyRecorded(_)
    ));

    // One more than the bound is corrupt.
    let mut over = state.clone();
    let mut marker = template;
    marker["key"]["item"]["number"] = (MAX_MARKERS + 1).into();
    if let Some(list) = over["markers"].as_array_mut() {
        list.push(marker);
    }
    fs::write(&path, serde_json::to_vec(&over)?)?;
    let error = fixture.reopen().err().ok_or("over-full markers accepted")?;
    assert!(matches!(
        error.downcast::<Error>().map(|error| *error),
        Ok(Error::State(StateError::CorruptState(
            Corruption::LimitExceeded
        )))
    ));
    Ok(())
}

#[test]
fn corrupt_markers_are_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    let recorder = scheduled("gate")?;
    for number in [1, 2] {
        fixture.store.record_marker(
            key("merge-gate", pull_request(number)?, 'a', None)?,
            verdict(EvidenceVerdict::Pass),
            &recorder,
            at(1),
        )?;
    }
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let mut duplicate = valid.clone();
    duplicate["markers"][1]["key"] = valid["markers"][0]["key"].clone();
    let mut malformed = valid.clone();
    malformed["markers"][0]["fact"] = serde_json::json!({"type": "verdict", "verdict": "maybe"});
    let mut zero = valid.clone();
    zero["markers"][0]["key"]["item"]["number"] = 0.into();
    let cases: [(serde_json::Value, Expectation); 3] = [
        (duplicate, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(
                    Corruption::DuplicateWorkflowMarker
                ))
            )
        }),
        (malformed, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
        (zero, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
    ];
    for (corrupt, expected) in cases {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt markers were accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(expected(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    Ok(())
}

#[test]
fn a_verdict_can_be_superseded_explicitly_for_the_same_head() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', Some('b'))?;
    let fail = verdict(EvidenceVerdict::Fail);
    let pass = verdict(EvidenceVerdict::Pass);
    fixture
        .store
        .record_marker(gate.clone(), fail.clone(), &scheduled("tick-1")?, at(1))?;

    // fail -> pass: the current fact is replaced and the old one kept.
    let MarkerRecording::Superseded(marker) =
        fixture
            .store
            .supersede_marker(&gate, &fail, pass.clone(), &scheduled("tick-2")?, at(2))?
    else {
        return Err("expected a supersession".into());
    };
    assert_eq!(marker.fact(), &pass);
    assert_eq!(marker.recorded_by(), &scheduled("tick-2")?);
    assert_eq!(marker.recorded_at(), at(2));
    assert!(matches!(
        marker.history(),
        [prior] if prior.fact == fail && prior.recorded_at == at(1) && prior.superseded_at == at(2)
            && prior.recorded_by == scheduled("tick-1")?
    ));

    // The same fact again is a no-op, even through supersede.
    assert!(matches!(
        fixture.store.supersede_marker(&gate, &pass, pass.clone(), &scheduled("tick-3")?, at(3))?,
        MarkerRecording::AlreadyRecorded(unchanged) if unchanged == marker
    ));
    // A stale writer that still expects `fail` is refused.
    assert!(matches!(
        fixture.store.supersede_marker(
            &gate,
            &fail,
            verdict(EvidenceVerdict::Unavailable),
            &scheduled("stale")?,
            at(4)
        ),
        Err(Error::State(StateError::MarkerConflict))
    ));
    // pass -> fail works the same way, and survives a reopen.
    fixture
        .store
        .supersede_marker(&gate, &pass, fail.clone(), &scheduled("tick-5")?, at(5))?;
    let reopened = fixture.reopen()?.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(reopened.fact(), &fail);
    assert_eq!(
        reopened
            .history()
            .iter()
            .map(|prior| prior.fact.clone())
            .collect::<Vec<_>>(),
        [fail.clone(), pass.clone()]
    );
    // Silent conflicting writes are still refused.
    assert!(matches!(
        fixture
            .store
            .record_marker(gate, pass, &scheduled("tick-6")?, at(6)),
        Err(Error::State(StateError::MarkerConflict))
    ));
    Ok(())
}

#[test]
fn supersession_keeps_a_bounded_history_and_never_refuses_for_capacity() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', None)?;
    let facts = [EvidenceVerdict::Pass, EvidenceVerdict::Fail];
    fixture
        .store
        .record_marker(gate.clone(), verdict(facts[0]), &scheduled("tick")?, at(0))?;
    let rounds = kitchen::state::MAX_MARKER_HISTORY + 5;
    for round in 1..=rounds {
        let before = verdict(facts[(round - 1) % 2]);
        let after = verdict(facts[round % 2]);
        let seconds = u64::try_from(round)?;
        assert!(matches!(
            fixture.store.supersede_marker(
                &gate,
                &before,
                after,
                &scheduled("tick")?,
                at(seconds)
            )?,
            MarkerRecording::Superseded(_)
        ));
    }
    let marker = fixture.store.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(marker.history().len(), kitchen::state::MAX_MARKER_HISTORY);
    assert_eq!(marker.dropped_history(), 5);
    // The oldest kept entry is the one replaced in round 6.
    assert_eq!(
        marker.history().first().map(|prior| prior.superseded_at),
        Some(at(6))
    );
    Ok(())
}

#[test]
fn questions_are_not_superseded_and_missing_markers_cannot_be() -> TestResult {
    let fixture = Fixture::new()?;
    let triage = key("triage", pull_request(7)?, 'a', None)?;
    let asked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-1")?,
    };
    fixture
        .store
        .record_marker(triage.clone(), asked.clone(), &scheduled("tick")?, at(1))?;
    let reasked = MarkerFact::QuestionAsked {
        question: ExternalRef::new("roger-ask-2")?,
    };
    assert!(matches!(
        fixture
            .store
            .supersede_marker(&triage, &asked, reasked, &scheduled("tick")?, at(2)),
        Err(Error::State(StateError::MarkerNotSupersedable))
    ));
    let missing = key("merge-gate", pull_request(8)?, 'a', None)?;
    assert!(matches!(
        fixture.store.supersede_marker(
            &missing,
            &verdict(EvidenceVerdict::Fail),
            verdict(EvidenceVerdict::Pass),
            &scheduled("tick")?,
            at(3)
        ),
        Err(Error::State(StateError::MarkerNotFound))
    ));
    assert_eq!(fixture.store.marker(&missing)?, None);
    Ok(())
}

#[test]
fn corrupt_marker_history_is_rejected_without_reset() -> TestResult {
    let fixture = Fixture::new()?;
    let gate = key("merge-gate", pull_request(20)?, 'a', None)?;
    let fail = verdict(EvidenceVerdict::Fail);
    fixture
        .store
        .record_marker(gate.clone(), fail.clone(), &scheduled("tick")?, at(1))?;
    fixture.store.supersede_marker(
        &gate,
        &fail,
        verdict(EvidenceVerdict::Pass),
        &scheduled("tick")?,
        at(2),
    )?;
    let path = fixture.state_path();
    let valid: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let entry = valid["markers"][0]["history"][0].clone();
    let mut over = valid.clone();
    over["markers"][0]["history"] =
        vec![entry.clone(); kitchen::state::MAX_MARKER_HISTORY + 1].into();
    let mut malformed = valid.clone();
    malformed["markers"][0]["history"][0]["supersededAt"] = "yesterday".into();
    let cases: [(serde_json::Value, Expectation); 2] = [
        (over, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::LimitExceeded))
            )
        }),
        (malformed, |error| {
            matches!(
                error,
                Error::State(StateError::CorruptState(Corruption::Syntax { .. }))
            )
        }),
    ];
    for (corrupt, expected) in cases {
        let bytes = serde_json::to_vec_pretty(&corrupt)?;
        fs::write(&path, &bytes)?;
        let error = fixture
            .reopen()
            .err()
            .ok_or("corrupt history was accepted")?;
        let error = error
            .downcast::<Error>()
            .map_err(|_| "unexpected error type")?;
        assert!(expected(&error), "{error:?}");
        assert_eq!(fs::read(&path)?, bytes, "rejected state is not rewritten");
    }
    // An exactly full history is valid.
    let mut full = valid;
    full["markers"][0]["history"] = vec![entry; kitchen::state::MAX_MARKER_HISTORY].into();
    fs::write(&path, serde_json::to_vec_pretty(&full)?)?;
    let marker = fixture.reopen()?.marker(&gate)?.ok_or("marker lost")?;
    assert_eq!(marker.history().len(), kitchen::state::MAX_MARKER_HISTORY);
    Ok(())
}
