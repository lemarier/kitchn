---
title: Two-minute setup
description: Register a house, pin its guidance and adopt a repository.
---

This walks through the three steps every setup needs. Nothing here starts a
worker, creates a label or uses a credential.

## 1. Describe your house

A house policy is a JSON file you review and keep. List the repositories kitchn
may work in, where it may post, and what every change needs.

```json title="house.json"
{
  "schema": 1,
  "house": "acme",
  "kitchen": "<kitchn commit>",
  "guidance": "<house guidance commit>",
  "repositories": ["acme/app"],
  "postingDestinations": ["acme/app"],
  "requiredReviewers": ["desktop-reviewer"],
  "requiredChecks": ["tauri-check", "specta-types"],
  "grants": [],
  "policyLimits": []
}
```

`kitchen` and `guidance` pin the exact kitchn and house-guidance revisions this
house runs under. Leave `grants` empty to start. See
[Houses](/docs/concepts/houses/) for every field.

## 2. Register it and pin the guidance

The registry lives outside any checkout, in a directory you control.

```sh
kitchn house init --registry ~/.kitchn --config house.json
kitchn house sync --registry ~/.kitchn --house acme --bundle verified-bundle.json
```

`sync` imports your house guidance from a verified bundle and pins it. The
bundle must match the revisions in `house.json`. Authenticating where the
bundle came from is your step, before `sync`.

## 3. Adopt a repository

Run this inside the repository:

```sh
kitchn house setup --registry ~/.kitchn --repository acme/app
```

Setup asks which house and which workflows to use (`none` is fine for
interactive-only work), writes `.kitchen.json` at the Git root and prints a
doctor report.

```text
Adopted .kitchen.json for acme/app.
House: acme
Repository: acme/app
Doctor: setup incomplete
...
No workers, schedules, labels, or external actions were activated.
```

"Setup incomplete" is expected at this point. Doctor lists each missing piece,
such as repository access it hasn't observed or backend capabilities a workflow
needs, with the next step for each. Anything doctor can't observe is reported
as unknown, never as fine.

## Next

- Start new repositories from a house template: [Templates](/docs/guides/templates/).
- Update pinned guidance safely: [Pins and recovery](/docs/guides/pins-and-recovery/).
- Every command and exit code: [CLI reference](/docs/reference/cli/).
