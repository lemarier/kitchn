//! The JSON bodies of the HTTP worker protocol, and their conversion into
//! contract types. Every response is parsed here once; nothing outside this
//! module reads raw JSON.

use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    BackendId, HouseId,
    contracts::{
        BackendDescriptor, Capability, CapabilitySet, Delivery, EffectFailure, EffectRequest,
        ExternalRef, Liveness, Lookup, MAX_TEXT_BYTES, MailMessage, MessageKind, NotAppliedReason,
        Receipt, ResourceObservation, ResourceRef, Support, Text, UncertainReason, WorkerOutcome,
        WorkerState,
    },
    scheduling::AgentFamily,
    selection::{EffortSupport, SelectionSupport},
    state::UsageReport,
};

/// The capabilities this protocol can carry. A service may report others;
/// the adapter declares only these, since it has no calls for the rest.
pub(super) const COVERED: [Capability; 25] = [
    Capability::WorkerLaunchIsolated,
    Capability::WorkerLaunchReadiness,
    Capability::WorkerMessaging,
    Capability::WorkerStatusAndOutcome,
    Capability::WorkerCancel,
    Capability::WorkerDeliveries,
    Capability::RunTransfer,
    Capability::ResourceInventory,
    Capability::ResourceRelease,
    Capability::AgentSelectFamily,
    Capability::AgentSelectModel,
    Capability::UsageAttribution,
    Capability::HouseCredentials,
    Capability::EffectLookup,
    Capability::EffectIdempotentRequests,
    Capability::LookupLaunchWorker,
    Capability::IdempotentLaunchWorker,
    Capability::LookupMessageWorker,
    Capability::IdempotentMessageWorker,
    Capability::LookupReplyToWorker,
    Capability::IdempotentReplyToWorker,
    Capability::LookupCancelWorker,
    Capability::IdempotentCancelWorker,
    Capability::LookupReleaseResource,
    Capability::IdempotentReleaseResource,
];

/// `GET /v1/descriptor`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DescriptorBody {
    backend: BackendId,
    house: HouseId,
    capabilities: BTreeMap<String, Support>,
    #[serde(default)]
    worker_selection: Option<SelectionBody>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionBody {
    families: Vec<String>,
    #[serde(default)]
    model: bool,
    #[serde(default)]
    effort: EffortBody,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum EffortBody {
    #[default]
    Unsupported,
    WithModel,
    Always,
}

impl DescriptorBody {
    /// The descriptor the adapter declares: what the service reports,
    /// limited to [`COVERED`]. Unknown capability and family names are ignored, so a newer service
    /// still connects.
    pub(super) fn into_descriptor(self) -> BackendDescriptor {
        let capabilities = self
            .capabilities
            .iter()
            .filter_map(|(name, support)| Some((name.parse::<Capability>().ok()?, *support)))
            .filter(|(capability, _)| COVERED.contains(capability))
            .fold(CapabilitySet::new(), |set, (capability, support)| {
                set.with(capability, support)
            });
        let descriptor = BackendDescriptor {
            backend: self.backend,
            house: self.house,
            capabilities,
            worker_selection: None,
        };
        match self.worker_selection {
            None => descriptor,
            Some(selection) => descriptor.with_worker_selection(SelectionSupport {
                families: families(&selection.families),
                model: selection.model,
                effort: match selection.effort {
                    EffortBody::Unsupported => EffortSupport::Unsupported,
                    EffortBody::WithModel => EffortSupport::WithModel,
                    EffortBody::Always => EffortSupport::Always,
                },
            }),
        }
    }
}

/// The launchable families among `names`, as the static list
/// [`SelectionSupport`] holds.
fn families(names: &[String]) -> &'static [AgentFamily] {
    let has = |family: AgentFamily| names.iter().any(|name| name == family.as_str());
    match (has(AgentFamily::Claude), has(AgentFamily::Codex)) {
        (true, true) => &[AgentFamily::Claude, AgentFamily::Codex],
        (true, false) => &[AgentFamily::Claude],
        (false, true) => &[AgentFamily::Codex],
        (false, false) => &[],
    }
}

/// The body of `POST /v1/effects` and `POST /v1/effects/lookup`.
#[derive(Debug, Serialize)]
pub(super) struct EffectBody<'a> {
    pub run: &'a ExternalRef,
    pub request: &'a EffectRequest,
}

/// `POST /v1/effects`.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum ExecuteBody {
    Applied {
        receipt: Receipt,
    },
    NotApplied {
        reason: Refusal,
        #[serde(default)]
        capability: Option<Capability>,
        #[serde(default, rename = "retryAfterSeconds")]
        retry_after_seconds: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Refusal {
    Rejected,
    RateLimited,
    Unsupported,
    CrossHouse,
    ForeignBackend,
}

/// The outcome a `POST /v1/effects` response establishes.
///
/// A 200 response carries the outcome. 401 and 403 mean the service refused
/// the caller before acting, and 429 a rate limit before acting; every other
/// status, and a body that does not parse, leaves the outcome unknown.
pub(super) fn execute_outcome(
    status: u16,
    body: &[u8],
    request: &EffectRequest,
    backend: &BackendId,
) -> Result<Receipt, EffectFailure> {
    let lost = EffectFailure::Uncertain(UncertainReason::ResponseLost);
    match status {
        200 => {}
        401 | 403 => return Err(EffectFailure::NotApplied(NotAppliedReason::Rejected)),
        429 => {
            return Err(EffectFailure::NotApplied(NotAppliedReason::RateLimited {
                retry_after: None,
            }));
        }
        _ => return Err(lost),
    }
    match serde_json::from_slice::<ExecuteBody>(body).map_err(|_| lost)? {
        ExecuteBody::Applied { receipt } => {
            if names_only(&receipt, backend) {
                Ok(receipt)
            } else {
                Err(lost)
            }
        }
        ExecuteBody::NotApplied {
            reason,
            capability,
            retry_after_seconds,
        } => Err(EffectFailure::NotApplied(match reason {
            Refusal::Rejected => NotAppliedReason::Rejected,
            Refusal::RateLimited => NotAppliedReason::RateLimited {
                retry_after: retry_after_seconds.map(Duration::from_secs),
            },
            Refusal::Unsupported => NotAppliedReason::Unsupported(
                capability.unwrap_or_else(|| request.effect().required_capability()),
            ),
            Refusal::CrossHouse => NotAppliedReason::CrossHouse,
            Refusal::ForeignBackend => NotAppliedReason::ForeignBackend,
        })),
    }
}

/// Whether every resource in `receipt` belongs to `backend`.
fn names_only(receipt: &Receipt, backend: &BackendId) -> bool {
    receipt
        .created()
        .iter()
        .chain(receipt.touched())
        .all(|resource| &resource.backend == backend)
}

/// `POST /v1/effects/lookup`.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum LookupBody {
    Applied { receipt: Receipt },
    Absent,
    Unknown,
}

/// What a 200 lookup response establishes, or `None` for a body that does
/// not parse or a receipt naming another backend's resources.
pub(super) fn lookup(body: &[u8], backend: &BackendId) -> Option<Lookup> {
    Some(match serde_json::from_slice::<LookupBody>(body).ok()? {
        LookupBody::Applied { receipt } => {
            if !names_only(&receipt, backend) {
                return None;
            }
            Lookup::Applied(receipt)
        }
        LookupBody::Absent => Lookup::Absent,
        LookupBody::Unknown => Lookup::Unknown,
    })
}

/// The body naming one worker: `POST /v1/workers/observe` and
/// `POST /v1/workers/usage`.
#[derive(Debug, Serialize)]
pub(super) struct WorkerBody<'a> {
    pub worker: &'a ResourceRef,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum OutcomeBody {
    Succeeded,
    Failed,
    Cancelled,
}

impl From<OutcomeBody> for WorkerOutcome {
    fn from(outcome: OutcomeBody) -> Self {
        match outcome {
            OutcomeBody::Succeeded => Self::Succeeded,
            OutcomeBody::Failed => Self::Failed,
            OutcomeBody::Cancelled => Self::Cancelled,
        }
    }
}

/// `POST /v1/workers/observe`. A state Kitchen does not know reads as
/// [`WorkerState::Unknown`], never as readiness.
#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
enum StateBody {
    Starting,
    Ready,
    AwaitingReply,
    UserTakeover,
    Settled {
        outcome: OutcomeBody,
    },
    Missing,
    #[serde(other)]
    Unknown,
}

/// The worker state a 200 observe response reports, or `None` when the body
/// does not parse.
pub(super) fn worker_state(body: &[u8]) -> Option<WorkerState> {
    Some(match serde_json::from_slice::<StateBody>(body).ok()? {
        StateBody::Starting => WorkerState::Starting,
        StateBody::Ready => WorkerState::Ready,
        StateBody::AwaitingReply => WorkerState::AwaitingReply,
        StateBody::UserTakeover => WorkerState::UserTakeover,
        StateBody::Settled { outcome } => WorkerState::Settled(outcome.into()),
        StateBody::Missing => WorkerState::Missing,
        StateBody::Unknown => WorkerState::Unknown,
    })
}

/// `GET /v1/inventory`.
#[derive(Debug, Deserialize)]
struct InventoryBody {
    resources: Vec<ObservationBody>,
}

#[derive(Debug, Deserialize)]
struct ObservationBody {
    resource: ResourceRef,
    #[serde(default)]
    owner: Option<ExternalRef>,
    liveness: LivenessBody,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LivenessBody {
    Live,
    Exited,
    #[serde(other)]
    Unverifiable,
}

/// The observations a 200 inventory response lists, or `None` when the body
/// does not parse or names another backend's resource.
pub(super) fn inventory(body: &[u8], backend: &BackendId) -> Option<Vec<ResourceObservation>> {
    let listed: InventoryBody = serde_json::from_slice(body).ok()?;
    listed
        .resources
        .into_iter()
        .map(|observation| {
            (&observation.resource.backend == backend).then_some(ResourceObservation {
                resource: observation.resource,
                owner: observation.owner,
                liveness: match observation.liveness {
                    LivenessBody::Live => Liveness::Live,
                    LivenessBody::Exited => Liveness::Exited,
                    LivenessBody::Unverifiable => Liveness::Unverifiable,
                },
            })
        })
        .collect()
}

/// The body of every mailbox call.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MailboxBody<'a> {
    pub run: &'a ExternalRef,
    pub coordinator: &'a ExternalRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<&'a ExternalRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

/// The mailbox answer.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum MailboxAnswer {
    Delivery { delivery: DeliveryBody },
    Empty,
    Fenced,
}

#[derive(Debug, Deserialize)]
struct DeliveryBody {
    id: String,
    messages: Vec<Value>,
    #[serde(default)]
    unreadable: usize,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    id: String,
    kind: String,
    #[serde(default)]
    worker: Option<ResourceRef>,
    #[serde(default)]
    outcome: Option<OutcomeBody>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// What a 200 adoption response says.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Adoption {
    /// This coordinator now reads the run's mailbox.
    Adopted,
    /// Another coordinator adopted the run after this one.
    Fenced,
}

/// Parse a 200 adoption response, or `None` for any body other than the two
/// the protocol defines.
pub(super) fn adoption(body: &[u8]) -> Option<Adoption> {
    // Matched as exact objects: an explicit `null` status, an extra field,
    // or any other value is not one of the two answers.
    let object = serde_json::from_slice::<serde_json::Map<String, Value>>(body).ok()?;
    if object.is_empty() {
        return Some(Adoption::Adopted);
    }
    match (object.len(), object.get("status")) {
        (1, Some(Value::String(status))) if status == "fenced" => Some(Adoption::Fenced),
        _ => None,
    }
}

/// What a 200 mailbox response says.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Mailbox {
    /// The oldest unacknowledged batch, or `None` when nothing waits.
    Batch(Option<Delivery>),
    /// Another coordinator adopted the run.
    Fenced,
}

/// Parse a 200 mailbox response, or `None` when it or its batch id does not
/// parse. A message row that does not parse, or has an invalid id, is
/// dropped and counted as unreadable, so the batch is never acknowledged
/// blindly.
pub(super) fn mailbox(body: &[u8], backend: &BackendId) -> Option<Mailbox> {
    let delivery = match serde_json::from_slice::<MailboxAnswer>(body).ok()? {
        MailboxAnswer::Empty => return Some(Mailbox::Batch(None)),
        MailboxAnswer::Fenced => return Some(Mailbox::Fenced),
        MailboxAnswer::Delivery { delivery } => delivery,
    };
    let id = ExternalRef::new(&delivery.id).ok()?;
    let mut unreadable = delivery.unreadable;
    let messages = delivery
        .messages
        .into_iter()
        .filter_map(|row| {
            let message = serde_json::from_value::<MessageBody>(row)
                .ok()
                .and_then(|message| mail_message(message, backend));
            if message.is_none() {
                unreadable = unreadable.saturating_add(1);
            }
            message
        })
        .collect();
    Some(Mailbox::Batch(Some(Delivery {
        id,
        messages,
        unreadable,
    })))
}

fn mail_message(message: MessageBody, backend: &BackendId) -> Option<MailMessage> {
    let kind = match message.kind.as_str() {
        "question" => MessageKind::Question,
        "worker-done" => MessageKind::WorkerDone,
        "escalation" => MessageKind::Escalation,
        "heartbeat" => MessageKind::Heartbeat,
        "status" => MessageKind::Status,
        _ => MessageKind::Other,
    };
    Some(MailMessage {
        id: ExternalRef::new(&message.id).ok()?,
        kind,
        // A worker on another backend is not one this coordinator launched.
        worker: message.worker.filter(|worker| &worker.backend == backend),
        outcome: match kind {
            MessageKind::WorkerDone => message.outcome.map(WorkerOutcome::from),
            MessageKind::Question
            | MessageKind::Escalation
            | MessageKind::Heartbeat
            | MessageKind::Status
            | MessageKind::Other => None,
        },
        subject: bounded_text(message.subject.as_deref()),
        body: bounded_text(message.body.as_deref()),
    })
}

/// Untrusted text cut to Kitchen's bound on a character boundary, without
/// NUL bytes; `None` when nothing valid is left.
fn bounded_text(value: Option<&str>) -> Option<Text> {
    let value = value?.replace('\0', "");
    let mut end = value.len().min(MAX_TEXT_BYTES);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.get(..end).and_then(|text| Text::new(text).ok())
}

/// `POST /v1/workers/usage`.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum UsageBody {
    Reported { report: UsageReport },
    None,
}

/// The report a 200 usage response carries, `Some(None)` when the service
/// has none, or `None` when the body does not parse.
pub(super) fn usage(body: &[u8]) -> Option<Option<UsageReport>> {
    Some(match serde_json::from_slice::<UsageBody>(body).ok()? {
        UsageBody::Reported { report } => Some(report),
        UsageBody::None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> Result<BackendId, crate::IdentifierError> {
        BackendId::new("sandbox")
    }

    #[test]
    fn a_descriptor_keeps_only_covered_capabilities() -> Result<(), Box<dyn std::error::Error>> {
        let body: DescriptorBody = serde_json::from_str(
            r#"{"backend":"sandbox","house":"origin89","capabilities":{
                "worker.launch_isolated":"supported","worker.messaging":"partial",
                "schedule.manage":"supported","forge.mutation":"supported",
                "effect.idempotent.release_resource":"supported","not.a.capability":"supported"},
                "workerSelection":{"families":["codex","gpt"],"model":true,"effort":"with-model"}}"#,
        )?;
        let descriptor = body.into_descriptor();
        let capabilities = &descriptor.capabilities;
        assert_eq!(
            capabilities.support(Capability::WorkerLaunchIsolated),
            Some(Support::Supported)
        );
        assert_eq!(
            capabilities.support(Capability::WorkerMessaging),
            Some(Support::Partial)
        );
        assert!(capabilities.supports(Capability::IdempotentReleaseResource));
        // Reported, but the protocol has no calls for them.
        assert_eq!(capabilities.support(Capability::ScheduleManage), None);
        assert_eq!(capabilities.support(Capability::ForgeMutation), None);
        let selection = descriptor.worker_selection.ok_or("no selection")?;
        assert_eq!(selection.families, &[AgentFamily::Codex]);
        assert!(selection.model);
        assert_eq!(selection.effort, EffortSupport::WithModel);
        Ok(())
    }

    #[test]
    fn an_invalid_support_level_rejects_the_descriptor() {
        assert!(
            serde_json::from_str::<DescriptorBody>(
                r#"{"backend":"sandbox","house":"origin89","capabilities":{"worker.cancel":"yes"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_worker_states_are_unknown_not_ready() {
        assert_eq!(
            worker_state(br#"{"state":"ready"}"#),
            Some(WorkerState::Ready)
        );
        assert_eq!(
            worker_state(br#"{"state":"settled","outcome":"cancelled"}"#),
            Some(WorkerState::Settled(WorkerOutcome::Cancelled))
        );
        assert_eq!(
            worker_state(br#"{"state":"warming-up"}"#),
            Some(WorkerState::Unknown)
        );
        assert_eq!(worker_state(br#"{"state":"settled"}"#), None);
        assert_eq!(worker_state(b"not json"), None);
    }

    #[test]
    fn unreadable_mailbox_rows_are_counted_not_dropped_silently()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"status":"delivery","delivery":{"id":"d1","messages":[
            {"id":"m1","kind":"worker-done","outcome":"succeeded",
             "worker":{"kind":"worker","backend":"sandbox","handle":"w1"}},
            {"id":"m2","kind":"question","outcome":"failed",
             "worker":{"kind":"worker","backend":"elsewhere","handle":"w2"}},
            {"id":"","kind":"question"},
            {"kind":"escalation"},
            {"id":"m5","kind":"telemetry","subject":"a\u0000b"}]}}"#;
        let Some(Mailbox::Batch(Some(delivery))) = mailbox(body, &backend()?) else {
            return Err("no batch".into());
        };
        assert_eq!(delivery.unreadable, 2);
        let kinds: Vec<_> = delivery.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::WorkerDone,
                MessageKind::Question,
                MessageKind::Other
            ]
        );
        let [done, question, other] = delivery.messages.as_slice() else {
            return Err("three messages".into());
        };
        assert_eq!(done.outcome, Some(WorkerOutcome::Succeeded));
        assert!(done.worker.is_some());
        // An outcome only belongs on a report; a foreign worker is dropped.
        assert_eq!(question.outcome, None);
        assert_eq!(question.worker, None);
        assert_eq!(other.subject.as_ref().map(Text::as_str), Some("ab"));
        assert!(!delivery.is_idle());
        Ok(())
    }

    #[test]
    fn mailbox_answers_parse_or_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        let backend = backend()?;
        assert_eq!(
            mailbox(br#"{"status":"empty"}"#, &backend),
            Some(Mailbox::Batch(None))
        );
        assert_eq!(
            mailbox(br#"{"status":"fenced"}"#, &backend),
            Some(Mailbox::Fenced)
        );
        // A batch without a valid id cannot be acknowledged.
        assert_eq!(
            mailbox(
                br#"{"status":"delivery","delivery":{"id":"","messages":[]}}"#,
                &backend
            ),
            None
        );
        assert_eq!(mailbox(br#"{"status":"closed"}"#, &backend), None);
        Ok(())
    }

    #[test]
    fn only_the_protocols_adoption_answers_parse() {
        assert_eq!(adoption(b"{}"), Some(Adoption::Adopted));
        assert_eq!(adoption(br#"{"status":"fenced"}"#), Some(Adoption::Fenced));
        for refused in [
            &br#"{"status":"adopted"}"#[..],
            br#"{"status":null,"adopted":true}"#,
            br#"{"status":null}"#,
            br#"{"status":"fenced","extra":1}"#,
            br#"{"status":"empty"}"#,
            b"[]",
            b"",
            b"not json",
        ] {
            assert_eq!(
                adoption(refused),
                None,
                "{}",
                String::from_utf8_lossy(refused)
            );
        }
    }

    #[test]
    fn inventory_refuses_another_backends_resource() -> Result<(), Box<dyn std::error::Error>> {
        let backend = backend()?;
        let listed = inventory(
            br#"{"resources":[{"resource":{"kind":"worker","backend":"sandbox","handle":"w1"},
                "liveness":"sleeping"}]}"#,
            &backend,
        )
        .ok_or("inventory")?;
        assert_eq!(
            listed.first().map(|o| o.liveness),
            Some(Liveness::Unverifiable)
        );
        assert!(
            inventory(
                br#"{"resources":[{"resource":{"kind":"worker","backend":"other","handle":"w1"},
                    "liveness":"live"}]}"#,
                &backend,
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn usage_answers_parse() -> Result<(), Box<dyn std::error::Error>> {
        let report = usage(
            br#"{"status":"reported","report":{"source":"run-7","tokens":{"input":10,"output":2}}}"#,
        )
        .ok_or("usage")?
        .ok_or("report")?;
        assert_eq!(report.tokens.input, Some(10));
        assert_eq!(report.tokens.cache_read, None);
        assert_eq!(usage(br#"{"status":"none"}"#), Some(None));
        assert_eq!(usage(br#"{"status":"reported"}"#), None);
        Ok(())
    }
}
