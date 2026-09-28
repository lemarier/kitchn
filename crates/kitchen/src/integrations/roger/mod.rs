//! House, task, action, and revision-bound human decisions.
//!
//! [`RogerExecutor::effect`] prepares a payload for [`crate::state::run_effect`].
//! Persist the resulting receipt with the task and poll its Ask ID using
//! [`RogerClient::poll`]. Restart uses the same core effect name and idempotency
//! key; an offline read or unanswered request never creates a replacement Ask.
//! [`DecisionStatus::Approved`] satisfies only the recorded human-decision
//! condition. It does not grant task permissions or bypass other workflow gates.
//!
//! Roger is optional: [`RogerCli::detect`] checks the installed command locally.
//! Requester names are configuration choices and may be shared; house/task keys
//! still prevent answer routing across houses.
//! [`RogerCli`] requires a private credential binding and a known Ask owned by
//! that requester as an identity probe. The installed CLI has no identity-only
//! command, so missing probe evidence is a provisioning failure. A `get` may
//! record Roger's delivered trace; it never grants consent or creates an Ask.
//! Credentials and operations remain house scoped; unknown legacy decision
//! prefixes require explicit migration into the closed [`DecisionOwner`] set.

mod binding;
pub use crate::contracts::{AskKind, AskRisk, DecisionBinding, DecisionOwner, RogerAsk};
pub use binding::{DecisionStatus, validate_answer};
mod client;
pub use client::{RogerAvailability, RogerCli, RogerClient, RogerReadTransport};
mod executor;
mod provider;
pub use executor::RogerExecutor;
pub use provider::RogerMutationTransport;
