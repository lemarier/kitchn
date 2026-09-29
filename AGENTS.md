# Working in Kitchen

Read `CONTRIBUTING.md`. For local tasks, run `just skills-sync` and read
`skills/origin89-working/SKILL.md`, `origin89-rust/SKILL.md`,
`origin89-testing/SKILL.md`, and relevant domain skills under the immutable
`path` it prints. Keep that snapshot for this task. Before Git publication,
read `origin89-commits/SKILL.md` there. If refresh falls back to a verified
cache, report that fact; if no verified cache exists, report the missing shared
context and follow these local rules without claiming it loaded.

Origin89 engineering supplies the temporary development standards. Kitchen is
lemarier's project. Preserve third-party notices without attributing new Kitchen
work to another organization. Product roles and house rules must remain separate
from the standards used to develop this repository.

- `crates/kitchen`: reusable Rust domain contracts and workflow policy.
- `crates/kitchen-cli`: thin executable boundary (package and binary `kitchn`).
- `.origin89/`: temporary engineering bootstrap and its notices.
- `crates/*/tests/`: Rust integration tests.
- `.origin89/tests/`: temporary upstream bootstrap regression tests.
- `.github/workflows/check.yml`: host validation, without publication authority.

Use `just --list` for exact commands and `just check` for required validation.
Do not run skills refresh during hosted reviews; the ignored local cache is not
guaranteed to exist there. The rules below are checked in and apply even when
shared skills are unavailable. State that limitation.

Keep generic workflow decisions independent of Orca. Require explicit backend
capabilities; missing evidence or unsupported capabilities are not success.
House identity, credential access, posting destinations, task authority, and
decision revision must agree before an external effect. Installing Kitchen does
not grant authority. Keep private context and operational history house-scoped.

Honor issue dependencies and file ownership. Use one writer per branch and
coordinate shared contract/manifest changes. Do not start workers or schedules
merely because a role exists. Do not alter live Orca jobs or clean up sessions
without applicable authority. Live automation definitions are migration input;
the engineering Git repository contains only part of the guidance.

## Code Review Rules

Read `origin89-review` and relevant domain skills when available; never claim a
remote link or an ignored local cache was loaded by a hosted reviewer.

- Review the requested revision and real callers. Report trigger, consequence,
  and location; distinguish verified defects from speculation.
- Use typed state, IDs, units, errors, and exhaustive transitions. Validate
  external inputs; avoid production panics, stringly typed contracts, swallowed
  errors, unbounded retries, and locks held across unrelated I/O.
- Verify task ownership, durable intent, idempotency, uncertain external outcomes,
  cancellation, and restart recovery. Prevent duplicate writers/consumers.
- Require exact-revision evidence and scoped approvals. Reject cross-house
  access, stale approvals, secret exposure, and authority expansion.
- Installer changes must preserve local files and the last verified snapshot on
  failure. Check redirected paths and partial activation; legitimate managed
  skill links are intentional.
- Cleanup needs positive ownership and preservation evidence, rechecked before
  effects. Resource age or terminal silence alone never establishes safety.
- Require meaningful success, invalid-input, boundary, and failure/recovery tests
  for nontrivial behavior. Never confuse mocks, CI passes, or model agreement
  with live runtime or physical evidence.
- Workflows need bounded execution and minimal token permissions. Untrusted PR
  code must not receive base-repository secrets. Review-only work cannot publish
  or operate equipment. A review request authorizes findings, not fixes.
- Preserve licenses and notices, avoid duplicate findings, and report validation
  gaps. Hosted review following these rules must be observed, not inferred from
  syntax checks.
