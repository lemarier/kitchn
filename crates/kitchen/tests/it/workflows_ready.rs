//! Ready-to-merge reports: once per exact head, re-armed by a moved head or
//! base, never before the final review and the required checks agree at
//! that head. Temporary stores only.

use crate::common;

use common::{Fixture, TestResult, at, commit, scheduled};
use kitchen::{
    contracts::{EvidenceSubject, EvidenceVerdict, ExternalRef, IssueNumber, Repository},
    state::StateError,
    workflows::ready::{
        HeadEvidence, MergeReadiness, NotReady, ReadyDecision, ReadyReport, confirm_ready_reported,
        ready_to_merge,
    },
};

fn subject(head: char, base: char) -> TestResult<EvidenceSubject> {
    Ok(EvidenceSubject {
        head: commit(head)?,
        base: Some(commit(base)?),
    })
}

fn evidence(
    verdict: EvidenceVerdict,
    at: &EvidenceSubject,
    source: &str,
) -> TestResult<HeadEvidence> {
    Ok(HeadEvidence {
        verdict,
        subject: at.clone(),
        source: ExternalRef::new(source)?,
    })
}

fn green(head: char, base: char) -> TestResult<MergeReadiness> {
    let at = subject(head, base)?;
    Ok(MergeReadiness {
        repository: Repository::new("origin89hq/firmware")?,
        pull_request: IssueNumber::new(30)?,
        review: evidence(EvidenceVerdict::Pass, &at, "review-1")?,
        checks: evidence(EvidenceVerdict::Pass, &at, "checks-1")?,
        subject: at,
    })
}

fn report_of(decision: ReadyDecision) -> TestResult<ReadyReport> {
    match decision {
        ReadyDecision::Report(report) => Ok(report),
        other => Err(format!("expected a report, got {other:?}").into()),
    }
}

#[test]
fn a_ready_report_is_sent_once_per_head_after_delivery() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let coordinator = scheduled("coordinator")?;
    let ready = green('d', 'e')?;
    let report = report_of(ready_to_merge(store, &coordinator, &ready, at(10))?)?;
    assert_eq!(report.head, commit('d')?);
    assert_eq!(report.base, Some(commit('e')?));
    let message = report.message();
    for part in [
        "origin89hq/firmware#30",
        commit('d')?.as_str(),
        "review-1",
        "checks-1",
    ] {
        assert!(message.contains(part), "{message} names {part}");
    }
    // Not yet confirmed: a restarted coordinator reports again rather than never.
    assert_eq!(
        ready_to_merge(store, &coordinator, &ready, at(11))?,
        ReadyDecision::Report(report.clone())
    );
    confirm_ready_reported(store, &coordinator, &report, at(12))?;
    confirm_ready_reported(store, &coordinator, &report, at(13))?;
    assert_eq!(
        ready_to_merge(store, &coordinator, &ready, at(14))?,
        ReadyDecision::AlreadyReported
    );
    // The fact survives a reopened store.
    assert_eq!(
        ready_to_merge(&fixture.reopen()?, &coordinator, &ready, at(15))?,
        ReadyDecision::AlreadyReported
    );
    Ok(())
}

#[test]
fn a_moved_head_or_base_re_arms_the_report() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let coordinator = scheduled("coordinator")?;
    let first = report_of(ready_to_merge(
        store,
        &coordinator,
        &green('d', 'e')?,
        at(10),
    )?)?;
    confirm_ready_reported(store, &coordinator, &first, at(11))?;
    for moved in [green('f', 'e')?, green('d', 'a')?] {
        let report = report_of(ready_to_merge(store, &coordinator, &moved, at(12))?)?;
        assert_eq!(report.head, moved.subject.head);
        assert_eq!(report.base, moved.subject.base);
    }
    assert_eq!(
        ready_to_merge(store, &coordinator, &green('d', 'e')?, at(13))?,
        ReadyDecision::AlreadyReported
    );
    Ok(())
}

#[test]
fn nothing_is_reported_until_review_and_checks_agree_at_the_head() -> TestResult {
    let fixture = Fixture::new()?;
    let store = &fixture.store;
    let coordinator = scheduled("coordinator")?;
    let old = subject('c', 'e')?;
    let cases = [
        (
            MergeReadiness {
                review: evidence(EvidenceVerdict::Fail, &subject('d', 'e')?, "review-1")?,
                ..green('d', 'e')?
            },
            vec![NotReady::ReviewNotClean],
        ),
        (
            MergeReadiness {
                checks: evidence(
                    EvidenceVerdict::Unavailable,
                    &subject('d', 'e')?,
                    "checks-1",
                )?,
                ..green('d', 'e')?
            },
            vec![NotReady::ChecksNotGreen],
        ),
        (
            MergeReadiness {
                review: evidence(EvidenceVerdict::Pass, &old, "review-0")?,
                checks: evidence(EvidenceVerdict::Pass, &old, "checks-0")?,
                ..green('d', 'e')?
            },
            vec![NotReady::ReviewStale, NotReady::ChecksStale],
        ),
    ];
    for (readiness, reasons) in cases {
        assert_eq!(
            ready_to_merge(store, &coordinator, &readiness, at(10))?,
            ReadyDecision::NotReady(reasons)
        );
    }
    // No marker was recorded, so the head still reports once it is ready.
    assert!(matches!(
        ready_to_merge(store, &coordinator, &green('d', 'e')?, at(11))?,
        ReadyDecision::Report(_)
    ));
    Ok(())
}

#[test]
fn a_report_that_was_never_decided_cannot_be_confirmed() -> TestResult {
    let fixture = Fixture::new()?;
    let ready = green('d', 'e')?;
    let report = ReadyReport {
        repository: ready.repository.clone(),
        pull_request: ready.pull_request,
        head: ready.subject.head.clone(),
        base: ready.subject.base.clone(),
        review: ready.review.source.clone(),
        checks: ready.checks.source.clone(),
    };
    let error = confirm_ready_reported(&fixture.store, &scheduled("coordinator")?, &report, at(10))
        .err()
        .ok_or("confirmed an unknown report")?;
    assert!(matches!(
        error,
        kitchen::Error::State(StateError::MarkerNotFound)
    ));
    Ok(())
}
