---
title: Introduction
description: What kitchn is, what it does today, and what it is being built to do.
---

kitchn is a workflow layer for coding agents. It decides who picks up an
issue, who works on it, who checks it and what is allowed to ship. The work
itself runs on an orchestrator you already use; Orca is the first one.

kitchn borrows the kitchen brigade: each task goes to a station with one
owner, an independent expediter checks the exact revision before anything
leaves the pass, and trust is earned from recorded evidence rather than
assumed.

## The pieces

- **House**: the organization kitchn works for. It owns the engineering rules,
  repository allowlist, reviewers, credentials and posting limits. See
  [Houses](/docs/concepts/houses/).
- **Roles**: eight stations, each with a responsibility, the evidence it owes
  and a boundary. See [The brigade](/docs/concepts/roles/).
- **Pinned guidance**: a house's rules are imported as a verified snapshot and
  pinned by revision, so every task knows exactly which rules it ran under.
- **Backends**: orchestrators that launch and supervise workers. A workflow
  names the capabilities it needs; a missing capability blocks the workflow.

## What works today

kitchn is early. The CLI can:

- register a house with a few prompts, or from a reviewed policy file,
- import and pin house guidance from a verified bundle,
- adopt a repository by recording its binding in your registry, without writing
  anything into the repository,
- diagnose what is still missing with `doctor`,
- scaffold or adopt repositories from house templates without overwriting
  local files,
- run an issue, a pull request or an issue draft from inside your agent session
  with the [`/kitchn` skill](/docs/guides/sessions/),
- preview cleanup, project decomposition and schedule budgets before anything
  acts.

Pickup, the merge gate, triage, the trust ledger and scheduled runs exist in
the library but do not run unattended yet.
[Project status](/docs/project/status/) lists what is done and what is next.

## What kitchn never does on its own

Installing kitchn starts no workers or schedules and grants no authority.
Posting, merging, publishing and cleanup each need their own explicit grant
from the house. See [Authority](/docs/concepts/authority/).
