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
