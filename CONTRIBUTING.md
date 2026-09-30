# Contributing to Kitchen

Kitchen is maintained by lemarier. During bootstrap, use the current Origin89
engineering standards through `just skills-sync` and the local rules in
[AGENTS.md](AGENTS.md). This is a development dependency, not Origin89 ownership
of Kitchen. Required notices on reused material remain intact.

Inspect existing contracts and callers before editing. Keep each issue and PR
focused, preserve unrelated changes, and use the owning issue's dependencies and
scope. Coordinate root workspace changes; do not create empty crates for
speculative future features.

Use enums for closed states, validated identifiers and units, and structured
errors. Parse external data at the boundary. Keep orchestration-specific I/O
separate from domain decisions. Expected I/O failures must not panic or silently
become successful outcomes. Bound retries, queues, subprocesses, and waits;
define cancellation, ownership, and recovery before adding background work.

For nontrivial behavior, test distinct success, invalid input, boundaries, and
failure/recovery paths. Add regression tests that demonstrate bugs before fixes
where practical. Simple contracts need only their meaningful cases. Use isolated
fixtures and a fake backend for offline tests; report real runtime evidence
separately. Check the current stable version and compatibility before adding a
dependency, retain the lockfile, and test the declared MSRV.

Run `just fmt`, then `just check`. Checks include formatting, Clippy, Rust and
temporary upstream bootstrap tests, builds, documentation, MSRV, and workflow syntax. Install tools
explicitly; checks do not install them or perform credentialed operations.
Test changed executable templates in a disposable consumer. Do not weaken a
check to make a change pass.

Use short conventional commit and PR titles and branches such as
`lemarier/task-name`. Preserve Git authorship and signing settings. Do not add
assistant attribution or co-author footers. Preparing work does not authorize
committing, pushing, merging, publishing, or activating jobs.

Keep documentation useful to contributors or operators. Put design discussion
and acceptance evidence in the owning issue or PR; do not commit session logs,
private automation exports, credentials, or stale inventories. Track confirmed
out-of-scope defects in the owning repository when posting is authorized.

Kitchen will adopt its own maintained operating rules after the integrated
workflows are validated. Keep the engineering bootstrap until equivalent rules
are available and the migration has been reviewed.

## Module ownership

The owning issue changes this map when an implementation moves. Paths below are
reserved ownership boundaries, not a request to create empty modules. Keep shared
exports in `crates/kitchen/src/lib.rs` coordinated with #4. The #3 bootstrap owns
root manifests and the lockfile until it lands; afterward, any worker needing a
dependency coordinates the root manifest and lockfile with the other active owners.

| Owner | Library paths under `crates/kitchen/src/` | Other boundaries |
| --- | --- | --- |
| #4 core contracts and durable ownership | `id.rs`, `error.rs`, `contracts/` (except payload files owned by #6 and #7), `state/` | Shared exports; state-store integration tests |
| #5 house configuration and adoption | `house/`, `adoption/` | House/repository config adoption and safe-write installer; role cards and instruction assets in root `roles/` |
| #6 Orca adapter and scheduling | `adapters/orca/`, `scheduling/`, `contracts/effects/schedule.rs` | Backend execution only; generic capability contracts belong to #4 |
| #7 GitHub and Roger | `integrations/github/` (except `app.rs`, owned by #193), `integrations/roger/`, `contracts/effects/github.rs`, `contracts/effects/roger.rs` | House-scoped external access |
| #8 pickup, coordination and repair | `workflows/pickup.rs`, `workflows/coordination.rs`, `workflows/recovery.rs`, `workflows/ready.rs`, `workflows/repair.rs`, `workflows/push.rs`, `workflows/stack.rs` | Workflow integration tests |
| #9 exact-head gate | `workflows/gate.rs`, `workflows/gate/` | Gate evidence and approval tests |
| #10 triage and gardener | `workflows/triage.rs`, `workflows/gardener.rs` | Hygiene tests |
| #11 dishwasher | `workflows/cleanup.rs` | Ownership and preservation tests |
| #12 trust and inspector | `trust/`, `workflows/inspector.rs` | Evidence and autonomy tests |
| #49 post-merge inspection sampling | `workflows/sampling.rs` | Sampling tests in `crates/kitchen/tests/inspection_sampling.rs`; sampling marker rules in `state/retention.rs`; sample result time in `workflows/inspector.rs`; trust records and inspections from #12, merge grants from #9, house budgets from #40 |
| #44 graduation policy | `trust/graduation.rs` | The house `graduation` policy field in `house/config.rs`; graduation retention rules in `trust/archive.rs`; graduation tests in `crates/kitchen/tests/graduation.rs`; trust records from #12 |
| #106, #134 schedule budget pass and tick | `workflows/budget.rs`, `kitchen-cli/src/commands/budget.rs` | Budget tests in `crates/kitchen/tests/schedule_budgets.rs`, `crates/kitchen-cli/tests/budget.rs` |
| #190 budget window task per window length | The window task choice and overlapping-window reconciliation in `workflows/budget.rs`; `exhausted_schema` in `scheduling/budget.rs` | Length-change tests in `crates/kitchen/tests/schedule_budgets.rs` |
| #13 end-to-end validation and operations | — | End-to-end harness in `crates/kitchen/tests/e2e_*`; operational docs in `docs/` |
| #16 house repository templates and scaffold/adopt flow | `scaffold/` | Template assets in root `templates/`; template rendering and repository scaffolding through #5's installer |
| #17 interactive entrypoints | `workflows/interactive.rs`, `workflows/interactive/` | The `/kitchn` skill in root `skills/kitchn/`; `work`, `pr`, `issue`, and `hand-back` in `crates/kitchen-cli/src/commands/interactive.rs`; interactive tests |
| #42 agent selection | `selection/` | House `agents` policy, the selection recorded on a task, and its launch check; backends map it to their own flags |
| #48 repository readiness | `house/readiness.rs`; readiness findings in `house/doctor.rs` | Readiness tests in `crates/kitchen/tests/house_readiness.rs`; check history comes from #7 and #9, grants from #12 |
| #50 deliberation threads and context records | `workflows/deliberation.rs`, `workflows/deliberation/` | Threads, records, and task pins as workflow markers in the house store; `crates/kitchen/tests/deliberation.rs` |
| #41 event-started workflows | `events/`; the event trigger in `contracts/trigger.rs` | Event intake tests; webhook delivery stays with #6 and #7 |
| #43 verification environments | `contracts/verification.rs` | Verification target, policy, evidence, and access tests in `crates/kitchen/tests/verification.rs` |
| #101 verification access binding and target-scoped grants | `contracts/verification.rs` with #43; `state/verification.rs` (`run_verification`) and the evidence-kind check in `HouseStore::record_evidence`; grant targets in `contracts/authority.rs` and `EvidenceKind::AuthorizedVerification` in `contracts/evidence.rs`, coordinated with #4 | Run, binding, forgery, target-scope, and legacy-grant tests in `crates/kitchen/tests/verification.rs` |
| #40 schedule intervals and usage budgets | `scheduling/budget.rs` | The house `schedules` policy field and schedule doctor findings in `house/`; install refusal in the Orca schedule adapter; `tests/schedule_budgets.rs` |
| #46 report intake | `workflows/intake.rs` | Intake source, grouping, and redaction tests |
| #192 backend deliveries and schedule inspection | `contracts/mailbox.rs`, `contracts/schedules.rs` | `run_mailbox` in `contracts/conformance.rs` and the fake's mailbox; `crates/kitchen/tests/backend_deliveries.rs` |
| #149 fake-executable test helper | `crates/kitchen/tests/common/executable.rs` | Test support only; `tests/fake_executable.rs` covers it |
| #189 fake-executable fork-pressure test | `crates/kitchen/tests/fake_executable.rs` | Test-only; separates the Linux `ETXTBSY` regression from resource failures on loaded hosts |
| #47 project decomposition | `workflows/decomposition.rs` | Decomposition tests; `kitchn decompose` CLI command |
| #98 guided house init | `house/wizard/` | `kitchn house init` without `--config` in `crates/kitchen-cli/src/commands/house_init.rs`; `tests/house_init.rs` in both crates |
| #173 house store created at init | `HouseRegistry::store_path` and `initialize_store` in `adoption/registry.rs`, with #5; the store step in `register_house` in `house/wizard/`, with #98 | `store_or_default` in `crates/kitchen-cli/src/commands/house.rs` and the `--store` default in each command that takes it; store tests in `tests/house_init.rs` in both crates and `crates/kitchen-cli/tests/interactive.rs` |
| #191 house worker backend binding | `house/backend.rs`, `adapters/resolve.rs` | The worker backend question in `house/wizard/`; `HouseRegistry::bind_backend`; backend construction in CLI commands such as `budget`; `tests/backend_binding.rs` |
| #140 house forge binding | `house/forge.rs` | The forge questions in `house/wizard/`; `kitchn forge` in `crates/kitchen-cli/src/commands/forge.rs`; the `ApprovedWrite` impls beside `apply_draft` and `decomposition::apply`, and the `issue apply`/`decompose apply` commands and acknowledgement re-reads in their command files; `tests/forge_binding.rs` and `crates/kitchen-cli/tests/forge.rs` |
| #193 GitHub App forge credentials | `integrations/github/app.rs` (carved out of #7) | `CredentialKind` and the app-installation refusal in `house/forge.rs`, coordinated with #140; the `TokenScope` carried by `ReadRequest`/`MutationRequest` and `GhCli`'s app auth, coordinated with #7; `--app-id`/`--installation` in `crates/kitchen-cli/src/commands/forge.rs`; tests in `crates/kitchen/tests/github_app.rs` |
| #81 workflow capability requirements at activation | The schedule's declared requirements in `scheduling/spec.rs`, their check in `HouseStore::begin_effect` and the Orca schedule install | Coordinated with #4 and #6; tests in `crates/kitchen/tests/workflow_activation.rs` |
| #85 house store retention | `state/retention.rs` | The retention policy and capacity report; `kitchn store` in `crates/kitchen-cli/src/commands/store.rs`; tests in `crates/kitchen/tests/state_retention.rs` and `crates/kitchen-cli/tests/store.rs`. Intake compaction and the doctor capacity finding stay with #46 and #48 |
| #161 open pull request effect | `GitHubAction::OpenPullRequest` in `contracts/effects/github.rs` and its provider arms in `integrations/github/`; layer opening in `workflows/stack.rs` | Effect tests in `crates/kitchen/tests/integrations_effects.rs`; opening tests in `crates/kitchen/tests/workflows_stack.rs` |
| #148 house follow-up policy | `house/config.rs` (`FollowUpPolicy`, `HouseConfig::follow_up_budget`) | `FollowUpBudget` can only come from the house config: `RepairPolicy::for_house` and `ForgeGatePolicy::for_house` (no scheduled repair or gate runner exists yet) and the interactive `pr` request in `workflows/interactive.rs` read it; the fix-round and review-request questions in `house/wizard/`; `tests/house_adoption.rs` |
| #130 agent inventory for house init | `adapters/orca/accounts.rs` | `AgentInventory` in `house/wizard/`; `PATH` and `orca account list` probes in `crates/kitchen-cli/src/commands/house_init.rs`; `tests/house_init.rs` |
| #119 task work type and derived trust scope | `TaskSpec::work_type` in `contracts/task.rs` (with #4); `StationScope::of_task` and binding checks in `trust/` (with #12) | The work type pickup, repair, and interactive rounds pass to `resolve_agent` and record; scope tests in `crates/kitchen/tests/trust_ledger.rs`; creation-to-binding tests in the pickup, repair, and interactive tests |
| #110 trust ledger archival | `trust/archive.rs` | The `archivals` summaries in `trust/store.rs` and the archive append in `state/snapshot.rs`; `kitchn trust` in `crates/kitchen-cli/src/commands/trust.rs`; tests in `crates/kitchen/tests/trust_archive.rs` and `crates/kitchen-cli/tests/trust.rs` |
| #107 merge trains | `workflows/train.rs` | `MergeReadiness::not_ready` in `workflows/ready.rs`, with #8; tests in `crates/kitchen/tests/workflows_train.rs` |
| #88 durable follow-ups for a person-held terminal | `workflows/follow_up.rs` | Held follow-up routing in `workflows/coordination.rs` (with #8); tests in `crates/kitchen/tests/workflows_follow_up.rs` |
| #174 user-facing repository links | — | `repository` in root `Cargo.toml`, README, and website links and clone commands; test fixtures keep `lemarier/kitchen` as sample data |
| #194 attempt usage records | `state/usage.rs` | The usage and pull-request fields on `AttemptRecord` and `TaskRecord` and their `HouseStore` methods in `state/model.rs` and `state/store.rs` (with #4); the usage count on retired tasks in `state/retention.rs` (with #85); tests in `crates/kitchen/tests/attempt_usage.rs` |
| #45 brigade audit | `workflows/audit.rs` | `kitchn audit` in `crates/kitchen-cli/src/commands/audit.rs`; tests in `crates/kitchen/tests/brigade_audit.rs` and `crates/kitchen-cli/tests/audit.rs`; reads trust records from #12, attempt usage from #194, and schedule budgets from #40 |
| #208 reply and usage recording from coordination | `AnswerSource`, the reply recording in `handle_question`, and `record_worker_usage` in `workflows/coordination.rs` (with #8) | Tests in `crates/kitchen/tests/workflows_coordination.rs` |
| #225 scheduled runner passes | `workflows/run.rs`, `workflows/run/` | `kitchn run` in `crates/kitchen-cli/src/commands/run.rs`; tests in `crates/kitchen/tests/run_passes.rs` and `crates/kitchen-cli/tests/run.rs`. The #218 tick invokes these commands |
| #218 house mailbox | `state/mailbox.rs` | The mailbox table, its store methods and the retention rule in `state/model.rs`, `state/store.rs` and `state/retention.rs` (with #4 and #85); `MailboxRoute` and the house mailbox brief line in `workflows/coordination.rs` (with #8); `kitchn mailbox` in `crates/kitchen-cli/src/commands/mailbox.rs`; tests in `crates/kitchen/tests/house_mailbox.rs` and `crates/kitchen-cli/tests/mailbox.rs` |
| #218 house tick and run ledger | `workflows/tick.rs`, `state/runs.rs` | The ledger table and its store methods in `state/model.rs` and `state/store.rs` (with #4); the `tick` field in `house/config.rs` (with #5); `kitchn tick` in `crates/kitchen-cli/src/commands/tick.rs`; tests in `crates/kitchen/tests/house_tick.rs` and `crates/kitchen-cli/tests/tick.rs`. The pass runners come from #225 |
| #195 HTTP worker backend | `adapters/http/` | `BackendKind::Http`, `HttpEndpoint`, and the binding's `endpoint` in `house/backend.rs` and `resolve_http_backend` in `adapters/resolve.rs` (with #191); the shared credential opener in `house/forge.rs` (with #140); tests in `crates/kitchen/tests/http_backend.rs` and the fake service in `crates/kitchen/tests/http_sim/`; the protocol reference `docs/reference/http-backend` on the website |

#5 owns house/repository config adoption and the safe-write installer. #16 owns
template assets, rendering, and repository scaffolding, built on #5's installer.

Each owner keeps its integration tests in `crates/kitchen/tests/` with a matching
area name. Coordinate shared `mod.rs`, exports, CLI command registration, and
manifests before editing; ownership of a leaf does not authorize competing edits
to those files. Keep domain decisions in the library. The CLI owns argument
parsing, presentation, and exit codes: 0 for success, 2 for invalid input, and 1
for execution or output failures. Scheduled prechecks follow the schedule
contract instead: 0 for actionable, 1 for idle, 2 for invalid input, and 3 for
read or output failures. Library errors must remain structured and must not echo
credentials or raw private input.

Shared parents follow these rules:

- Errors: each area owns its error type in its own module and adds one
  `#[from]` variant to `kitchen::Error` in `error.rs`, with its `class()` mapped
  to an `ErrorClass`. Callers such as the CLI branch on `Error::class()`, never on
  individual variants, so a new area never edits unrelated callers.
- Public paths: `lib.rs` declares each top-level module once. An area module is
  either public (`kitchen::contracts::…`) or private with root re-exports, never
  both. Submodules stay private and re-export through their parent `mod.rs`.
- `workflows/`: the first workflow PR to land creates `workflows/mod.rs` and its
  `pub mod workflows;` line in `lib.rs`. Later PRs rebase and add one line each.
  The same rule applies to `integrations/` and `adapters/`.
- CLI commands: each area adds `crates/kitchen-cli/src/commands/<area>.rs` with
  its subcommand enum and handler. The shared `Command` enum and dispatch
  gain one line per area; the first area to add a command creates
  `commands/mod.rs`.

## Bounded execution conventions

Before adding I/O, define a finite deadline, input/output byte limits, and the
owner of the operation. A retry policy must specify maximum attempts and elapsed
time, retryable errors, and what proves a previous effect did not occur. An
uncertain external result must be reconciled before retrying; cancellation does
not prove rollback. Persist intent and idempotency identity before durable or
external effects. Check cancellation before effects and between bounded waits,
and define how interrupted work resumes after restart. Never hold a state lock
across unrelated I/O.

## Commands and review context

From the repository root:

```sh
just --list
cargo fetch --locked
just fmt
just check
cargo test -p kitchen --locked --offline
cargo test -p kitchn --locked --offline
just msrv-check
just install
```

`just check` includes doctests through `just test`. `just install` uses the
committed lockfile and records the commit only from a clean tree on a
remote-tracking branch (see the README); repeat it from the same revision to
reinstall that version.
`just security` is a separate networked audit and requires its documented tools.

Local workers load the verified snapshot printed by `just skills-sync`, retain
that immutable path, and read the working, Rust, testing, writing, review and
commit skills there. An explicitly supplied verified snapshot takes precedence
for an active dispatched task. Hosted reviewers read the committed `AGENTS.md`
and this file at the reviewed revision: those are their reproducible local
baseline. They must state that the full shared skills are unavailable if their
snapshot was not supplied and verified; remote links and ignored caches are not
proof of loading those skills. Hosted reviews do not refresh the cache.
