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
| #241 GitHub App read tokens | `integrations/github/client.rs`, `integrations/github/app.rs`, `integrations/github/process.rs` | Repository-bound, read-only scopes for pickup and gate reads; credential wording in `crates/kitchen-cli/src/commands/forge.rs`; tests in `crates/kitchen/tests/github_app.rs` |
| #81 workflow capability requirements at activation | The schedule's declared requirements in `scheduling/spec.rs`, their check in `HouseStore::begin_effect` and the Orca schedule install | Coordinated with #4 and #6; tests in `crates/kitchen/tests/workflow_activation.rs` |
| #85 house store retention | `state/retention.rs` | The retention policy and capacity report; `kitchn store` in `crates/kitchen-cli/src/commands/store.rs`; tests in `crates/kitchen/tests/state_retention.rs` and `crates/kitchen-cli/tests/store.rs`. Intake compaction and the doctor capacity finding stay with #46 and #48 |
| #161 open pull request effect | `GitHubAction::OpenPullRequest` in `contracts/effects/github.rs` and its provider arms in `integrations/github/`; layer opening in `workflows/stack.rs` | Effect tests in `crates/kitchen/tests/integrations_effects.rs`; opening tests in `crates/kitchen/tests/workflows_stack.rs` |
| #148 house follow-up policy | `house/config.rs` (`FollowUpPolicy`, `HouseConfig::follow_up_budget`) | `FollowUpBudget` can only come from the house config: `RepairPolicy::for_house` and `ForgeGatePolicy::for_house` (read by the scheduled repair and gate passes of #225) and the interactive `pr` request in `workflows/interactive.rs` read it; the fix-round and review-request questions in `house/wizard/`; `tests/house_adoption.rs` |
| #130 agent inventory for house init | `adapters/orca/accounts.rs` | `AgentInventory` in `house/wizard/`; `PATH` and `orca account list` probes in `crates/kitchen-cli/src/commands/house_init.rs`; `tests/house_init.rs` |
| #119 task work type and derived trust scope | `TaskSpec::work_type` in `contracts/task.rs` (with #4); `StationScope::of_task` and binding checks in `trust/` (with #12) | The work type pickup, repair, and interactive rounds pass to `resolve_agent` and record; scope tests in `crates/kitchen/tests/trust_ledger.rs`; creation-to-binding tests in the pickup, repair, and interactive tests |
| #110 trust ledger archival | `trust/archive.rs` | The `archivals` summaries in `trust/store.rs` and the archive append in `state/snapshot.rs`; `kitchn trust` in `crates/kitchen-cli/src/commands/trust.rs`; tests in `crates/kitchen/tests/trust_archive.rs` and `crates/kitchen-cli/tests/trust.rs` |
| #107 merge trains | `workflows/train.rs` | `MergeReadiness::not_ready` in `workflows/ready.rs`, with #8; tests in `crates/kitchen/tests/workflows_train.rs` |
| #88 durable follow-ups for a person-held terminal | `workflows/follow_up.rs` | Held follow-up routing in `workflows/coordination.rs` (with #8); tests in `crates/kitchen/tests/workflows_follow_up.rs` |
| #174 user-facing repository links | — | `repository` in root `Cargo.toml`, README, and website links and clone commands; test fixtures keep `lemarier/kitchen` as sample data |
| #194 attempt usage records | `state/usage.rs` | The usage and pull-request fields on `AttemptRecord` and `TaskRecord` and their `HouseStore` methods in `state/model.rs` and `state/store.rs` (with #4); the usage count on retired tasks in `state/retention.rs` (with #85); tests in `crates/kitchen/tests/attempt_usage.rs` |
| #45 brigade audit | `workflows/audit.rs` | `kitchn audit` in `crates/kitchen-cli/src/commands/audit.rs`; tests in `crates/kitchen/tests/brigade_audit.rs` and `crates/kitchen-cli/tests/audit.rs`; reads trust records from #12, attempt usage from #194, and schedule budgets from #40 |
| #208 reply and usage recording from coordination | `AnswerSource`, the reply recording in `handle_question`, and `record_worker_usage` in `workflows/coordination.rs` (with #8) | Tests in `crates/kitchen/tests/workflows_coordination.rs` |
| #234 gate subject retry budget | `workflows/run/gate.rs` | The narrow `finish_attempt_exhausted` store transition in `state/model.rs` and `state/store.rs`, the subject marker retention rule in `state/retention.rs`; gate and retention cases in `crates/kitchen/tests/run_passes.rs` and `crates/kitchen/tests/state_retention.rs` |
| #230 independent gate attestation command | `attest_gate_review` in `workflows/run/attestation.rs` (with #225) | `kitchn gate attest` in `crates/kitchen-cli/src/commands/gate.rs`; fake forge and scheduled gate tests in `crates/kitchen/tests/run_passes.rs` |
| #238 forge gate review | `workflows/run/review.rs` and the review mutation in `contracts/effects/github.rs` and `integrations/github/` | `kitchn gate review` in `crates/kitchen-cli/src/commands/gate.rs`; fake forge and scheduled gate tests in `crates/kitchen/tests/run_passes.rs` |
| #225 scheduled runner passes | `workflows/run.rs`, `workflows/run/`, including the `gate.attestation` marker in `workflows/run/attestation.rs` | `kitchn run` in `crates/kitchen-cli/src/commands/run.rs`; `launch_rendered` in `workflows/coordination.rs` (with #8) and `check_standing`/`write_standing` in `workflows/pickup.rs`, shared by pickup and repair briefs; serde on `SemanticReview` and `RiskClass` in `workflows/gate.rs`; the stated checkout on worker reports (`CheckoutReport` in `EvidenceKind::WorkerReport` in `contracts/evidence.rs` with #4, `MailMessage::checkout` in `contracts/mailbox.rs` with #192, the report's `checkout` in `state/mailbox.rs`, `kitchn mailbox report --clean/--pushed`, and the house mailbox brief line in `workflows/coordination.rs` with #218); tests in `crates/kitchen/tests/run_passes.rs` and `crates/kitchen-cli/tests/run.rs`. The #218 tick runs these passes in process through `workflows/run/tick.rs` |
| #251 guided house authority | `HouseRegistry::configure_grants` in `adoption/registry.rs`; authority findings in `house/doctor.rs` | `kitchn house grant` and `kitchn house revoke` in `crates/kitchen-cli/src/commands/house_grant.rs`; CLI tests in `crates/kitchen-cli/tests/house_grant.rs` |
| #255 worker delivery | `workflows/push.rs` checked push and persisted PR opening; delivery hold in `workflows/run/coordinate.rs` | `kitchn push` in `crates/kitchen-cli/src/commands/push.rs`, absolute command in the worker brief, and focused push and coordination tests |
| #282 push preservation | Checked push checkout evidence in `workflows/push.rs` and `workflows/run/coordinate.rs`; exact-head preservation in `state/` and `workflows/run/repair.rs` | `kitchn preserve`, the worker report brief, and preservation tests |
| #286 preserved worktree reuse | Repair and follow-up worktree selection in `workflows/run/repair.rs`; checkout inspection in `contracts/backend.rs` and `adapters/orca/backend.rs` | Fake backend checkout observations and focused run tests |
| #218 house mailbox | `state/mailbox.rs` | The mailbox table, its store methods and the retention rule in `state/model.rs`, `state/store.rs` and `state/retention.rs` (with #4 and #85); `MailboxRoute` and the house mailbox brief line in `workflows/coordination.rs` (with #8); `kitchn mailbox` in `crates/kitchen-cli/src/commands/mailbox.rs`; tests in `crates/kitchen/tests/house_mailbox.rs` and `crates/kitchen-cli/tests/mailbox.rs` |
| #218 house tick and run ledger | `workflows/tick.rs`, `state/runs.rs` | The ledger table and its store methods in `state/model.rs` and `state/store.rs` (with #4); the `tick` field in `house/config.rs` (with #5); `kitchn tick` in `crates/kitchen-cli/src/commands/tick.rs`; tests in `crates/kitchen/tests/house_tick.rs` and `crates/kitchen-cli/tests/tick.rs`. The pass runner, `workflows/run/tick.rs`, is shared with #225; its tests are the tick tests in `crates/kitchen/tests/run_passes.rs` |
| #195 HTTP worker backend | `adapters/http/` | `BackendKind::Http`, `HttpEndpoint`, and the binding's `endpoint` in `house/backend.rs` and `resolve_http_backend` in `adapters/resolve.rs` (with #191); the shared credential opener in `house/forge.rs` (with #140); tests in `crates/kitchen/tests/http_backend.rs` and the fake service in `crates/kitchen/tests/http_sim/`; the protocol reference `docs/reference/http-backend` on the website |
| #228 tick backend host facts | `house/runtime.rs` | `RuntimeError` in `error.rs` (with #218) and `trust/store.rs`; the stored-runtime read in `crates/kitchen-cli/src/commands/run.rs` and the `trigger` flags in `commands/tick.rs` (with #225 and #218); tests in `crates/kitchen/tests/house_runtime.rs` and `crates/kitchen-cli/tests/tick_runtime.rs` |
| #242 workflow precheck failure causes | `WorkflowError::PrecheckFailed` and `known` in `workflows/mod.rs`; marker read mapping in `workflows/triage.rs` and `workflows/gardener.rs` | Gardener precheck CLI error in `crates/kitchen-cli/src/commands/gardener.rs`; cause tests in `crates/kitchen/tests/workflows_triage_gardener.rs` and `crates/kitchen-cli/tests/gardener.rs` |
| #240 checkout scope defaults | — | Shared CLI argument resolution in `crates/kitchen-cli/src/commands/defaults.rs`, command help, and `crates/kitchen-cli/tests/defaults.rs`; the typed missing-flag error in `house/error.rs` |
| #256 gate checkout inference | `adoption/remote.rs` clean checkout head and `integrations/github/client.rs` open PR read | Shared gate identifier resolution in `crates/kitchen-cli/src/commands/defaults.rs`, `gate review` and `gate attest` in `commands/gate.rs`, and gate CLI tests in `crates/kitchen-cli/tests/run.rs` |
| #271 coordinator recovery and merged delivery | `adapters/orca/inspect.rs` stale terminal diagnosis; merged PR settlement in `workflows/run/coordinate.rs` | Read-only doctor probe in `crates/kitchen-cli/src/commands/house.rs`; merge evidence in `contracts/evidence.rs`; focused run and CLI tests |
| #269 house writer identity | The app bot ID and worker identity in `house/forge.rs`; pre-start worktree identity in `adapters/orca/backend.rs`; commit author and committer check in `workflows/push.rs` | `kitchn forge bind` and `kitchn push`; Orca adapter, forge, and push tests; expediter and CLI guidance |

#5 owns house/repository config adoption and the safe-write installer. #16 owns
template assets, rendering, and repository scaffolding, built on #5's installer.

Each owner keeps its integration tests in `crates/kitchen/tests/` with a matching
area name. Coordinate shared `mod.rs`, exports, CLI command registration, and
manifests before editing; ownership of a leaf does not authorize competing edits
to those files. Keep domain decisions in the library. The CLI owns argument
parsing, presentation, and exit codes: 0 for success, 2 for invalid input, and 1
for execution or output failures. Scheduled prechecks follow the schedule
contract instead: 0 for actionable, 1 for idle, 2 for invalid input, and 3 for
read or output failures. `kitchn run` passes use their own set: 0 when the pass
acted or was idle, 2 for invalid input, 1 for execution or output failures, 3
when another start holds the pass lease (busy, not a read failure as for a
precheck), and 4 when the lease owner is uncertain. Library errors must remain
structured and must not echo credentials or raw private input.

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

Worker delivery uses `kitchn push` from its launched worktree. The command gives
the forge credential only to its checked Git push child, keeps the accepted
remote head in the house store for later pushes, and confirms a PR is still
open at the submitted head before linking it. See `skills/kitchn/SKILL.md` for
the worker procedure.

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
