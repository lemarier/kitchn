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
| #7 GitHub and Roger | `integrations/github/`, `integrations/roger/`, `contracts/effects/github.rs`, `contracts/effects/roger.rs` | House-scoped external access |
| #8 pickup, coordination and repair | `workflows/pickup.rs`, `workflows/coordination.rs`, `workflows/repair.rs` | Workflow integration tests |
| #9 exact-head gate | `workflows/gate.rs` | Gate evidence and approval tests |
| #10 triage and gardener | `workflows/triage.rs`, `workflows/gardener.rs` | Hygiene tests |
| #11 dishwasher | `workflows/cleanup.rs` | Ownership and preservation tests |
| #12 trust and inspector | `trust/`, `workflows/inspector.rs` | Evidence and autonomy tests |
| #13 end-to-end validation and operations | — | End-to-end harness in `crates/kitchen/tests/e2e_*`; operational docs in `docs/` |
| #16 house repository templates and scaffold/adopt flow | `scaffold/` | Template assets in root `templates/`; template rendering and repository scaffolding through #5's installer |
| #42 agent selection | `selection/` | House `agents` policy, the selection recorded on a task, and its launch check; backends map it to their own flags |

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
cargo test -p kitchen-cli --locked --offline
just msrv-check
cargo install --path crates/kitchen-cli --locked --offline
```

`just check` includes doctests through `just test`. The install command uses the
committed lockfile; repeat it from the same revision to reinstall that version.
`just security` is a separate networked audit and requires its documented tools.

Local workers load the verified snapshot printed by `just skills-sync`, retain
that immutable path, and read the working, Rust, testing, writing, review and
commit skills there. An explicitly supplied verified snapshot takes precedence
for an active dispatched task. Hosted reviewers read the committed `AGENTS.md`
and this file at the reviewed revision: those are their reproducible local
baseline. They must state that the full shared skills are unavailable if their
snapshot was not supplied and verified; remote links and ignored caches are not
proof of loading those skills. Hosted reviews do not refresh the cache.
