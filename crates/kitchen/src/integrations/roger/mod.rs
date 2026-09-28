//! House, task, action, and revision-bound human decisions.

mod binding;
pub use binding::{DecisionBinding, DecisionOwner, DecisionStatus, validate_answer};
mod client;
pub use client::{AskKind, AskRisk, RogerAsk, RogerCli, RogerClient, RogerReadTransport};
