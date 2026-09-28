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
