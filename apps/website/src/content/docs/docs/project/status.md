---
title: Project status
description: What kitchn does today and what is being built.
---

kitchn is early. The full automation release is tracked in
[issue #1](https://github.com/lemarier/kitchn/issues/1).

## Working today

From the CLI:

- House registration, guided or from a reviewed policy ([#98](https://github.com/lemarier/kitchn/issues/98))
- Verified guidance import and revision pins, with recovery from interrupted updates
- Repository adoption, with bindings kept in the registry instead of the repository ([#93](https://github.com/lemarier/kitchn/issues/93))
- `doctor` reports for pins, access, labels, backend capabilities and merge readiness
- Repository templates with previews, provenance and conflict detection
- Forge binding: the GitHub account a house writes as, with the token kept in a file you place
- Interactive sessions through the [`/kitchn` skill](/docs/guides/sessions/): `work`, `pr`, `issue new`, `issue refine` and `hand-back`, sharing durable claims with scheduled runs ([#17](https://github.com/lemarier/kitchn/issues/17)). Commands that take `--store` need a house state store, and no command creates one yet.
- Dishwasher cleanup previews and digest-bound approvals, including a disk-space trigger
- Project decomposition previews with dependency and ownership checks
- The gardener's scheduled precheck
- Schedule usage budgets: a tick that pauses exhausted schedules and reports them

In the library, with tests, but not yet run unattended:

- Issue pickup, supervised coordination, PR repair and stack pushes
- The pass: an exact-head review and merge gate
- Triage and gardener issue hygiene
- The trust ledger, scoped autonomy grants and the inspector
- The Orca adapter, forge-event intake and agent selection by role and work type

## Being built

- Validating the full automation and dogfooding kitchn on its own repositories ([#13](https://github.com/lemarier/kitchn/issues/13))
- Graduating workflows from supervised to unattended runs on evidence ([#44](https://github.com/lemarier/kitchn/issues/44))
- Merge trains for ready pull requests ([#107](https://github.com/lemarier/kitchn/issues/107))
- Posting approved drafts and decompositions from the CLI ([#140](https://github.com/lemarier/kitchn/issues/140))
- The rename to `kitchn` and the install script ([#18](https://github.com/lemarier/kitchn/issues/18))

[Open issues](https://github.com/lemarier/kitchn/issues) list the rest.
