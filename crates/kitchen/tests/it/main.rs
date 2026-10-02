//! Integration tests grouped to share one linker invocation.

#[path = "../common/mod.rs"]
mod common;
#[path = "../http_sim/mod.rs"]
mod http_sim;
#[path = "../orca_sim/mod.rs"]
mod orca_sim;
#[path = "../workflows_support/mod.rs"]
mod workflows_support;

mod agent_selection;
mod attempt_usage;
mod backend_binding;
mod backend_deliveries;
mod brigade_audit;
mod cleanup;
mod contracts;
mod decomposition;
mod deliberation;
mod effects;
mod events;
mod forge_binding;
mod gate;
mod github_app;
mod graduation;
mod house_adoption;
mod house_init;
mod house_mailbox;
mod house_readiness;
mod house_runtime;
mod house_tick;
mod http_backend;
mod identifiers;
mod inspection_sampling;
mod integrations_effects;
mod integrations_github;
mod integrations_roger;
mod interactive;
mod markers;
mod orca_adapter;
mod orca_process;
mod orca_recovery;
mod orca_signals;
mod push_preservation;
mod repository_binding;
mod run_passes;
mod scaffold;
mod scaffold_repository;
mod schedule_budgets;
mod scheduling;
mod state_retention;
mod state_store;
mod triggers;
mod trust_archive;
mod trust_ledger;
mod verification;
mod workflow_activation;
mod workflows_coordination;
mod workflows_follow_up;
mod workflows_intake;
mod workflows_pickup;
mod workflows_ready;
mod workflows_recovery;
mod workflows_repair;
mod workflows_stack;
mod workflows_train;
mod workflows_triage_gardener;
