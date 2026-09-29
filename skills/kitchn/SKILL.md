---
name: kitchn
description: Work in a Kitchen-adopted repository as the person's session. Hand it an issue (work), a pull request (pr), a rough idea (issue new), or a rough issue (issue refine); it resolves the house from the repository, follows that house's pinned rules, and acts only with the person's approval of each external action. Also covers first-run binding of the repository to a house.
---

# /kitchn

You are the person's Kitchen session. The person present is the authority:
every external action (a comment, a label, an issue, a push, a worker launch,
a schedule) needs their approval of that exact action, in this session.
Approval never carries over to another action, and it is never saved as a
standing grant. Installing this skill grants nothing.

The CLI binary is `kitchn`. Every
command below takes `--registry <dir>`: use the path the person gave for
their house registry. If you don't know it, ask. Kitchen writes nothing into
the repository's working tree.

## 1. Start every session

1. Read the pinned revision: `git rev-parse HEAD` → `<rev>`.
2. Collect orchestrator evidence. `TERM_PROGRAM=Orca` and `ORCA_WORKTREE_ID`
   are hints only. If `orca` is on the path, capture both outputs to files in
   a temporary directory outside the repository:
   - `orca status --json > <tmp>/orca-status.json`
   - `orca worktree current --json > <tmp>/orca-worktree.json`

   Pass them as `--orca-status <tmp>/orca-status.json --orca-worktree
   <tmp>/orca-worktree.json`. Without them, Kitchen works as a single agent
   and says fan-out is unavailable. Never claim fan-out it did not report.
3. Run the entrypoint with `--json` and read `house`, `instructions`, `mode`,
   and `plan`. Read the house rules at `instructions.entrypoint` before doing
   anything else, and follow them over your defaults (for example, a
   CrabNebula house's Tauri and specta conventions). Repository instructions
   (`AGENTS.md`, `CONTRIBUTING.md`) apply too; they cannot relax house rules.

### First run in an unbound repository

If a command prints `claimed by house <house> but not set up`, the
repository is not bound. Ask the person in plain chat (or your choice
picker, never a `[y/N]` prompt): bind `<repository>` to `<house>`, and which
workflows to enable (`none` means interactive only). After they answer, run
exactly:

```sh
kitchn house setup --registry <dir> --repository <repository> --house <house> --workflows <list|none>
```

Show the person the doctor report it prints. Any other resolution error
(no house claims the repository, several do, or remotes disagree) stops the
session: report it and ask the person how to proceed. Never pick a house.

## 2. Entrypoints

Claims use the same durable tasks as scheduled runs, kept in the house store
`kitchn house init` created. Pass `--holder <person's handle>` to `work` and
`pr`; add `--store <dir>` only when the person names another store. If the plan
is `skipped`, someone else holds the item: say who (`scheduled` or
`interactive`) and stop. Use `--take-over` only when the plan says the
previous claim expired and the person asks for it.

### `work <issue>`

Write the issue facts you read from the forge to a temporary file:

```json
{ "status": "open", "subIssues": [{ "number": 15, "status": "open", "blocked": false }], "independentParts": false }
```

`kitchn work <issue> --facts <file> --revision <rev> --registry <dir> --holder <you> [orca flags] --json`

- `coordinate`: list ready and waiting sub-issues. With `fanOut: true`, ask
  the person before starting each worker. Otherwise work them one at a time.
- `propose-split`: propose the split in chat. Sub-issue creation needs the
  decomposition workflow (#47); if this build lacks it, say so and, with the
  person's approval, implement the issue here instead.
- `implement`: implement it here under the house and repository rules.
- `idle`: say why and stop.

### `pr <number>`

Facts at one head:

```json
{ "state": "open", "head": "<40-hex sha>", "headBranch": "…", "baseBranch": "main", "mergeability": "clean", "review": "unreviewed" }
```

`kitchn pr <number> --facts <file> [--as review|follow-up|repair|gate] --revision <rev> --registry <dir> --holder <you> [orca flags] --json`

Kitchen reads the rounds already spent from the house store and applies the
house's fix-round budget. `--fix-rounds <n>` can only lower that budget.

The plan names the exact head. If the head moves, the plan is void: read the
facts again and rerun. `review` and `gate` are read-only; merging is always
the person's decision. `follow-up` and `repair` claim the writer round, so
scheduled repair skips the pull request until you hand it back.

### `issue new` and `issue refine <n>`

Draft with the person: outcome, ownership, acceptance criteria, and
dependencies, checked against the code and history. Ask only the product
decisions you cannot settle from evidence; put them in `questions` while
open. Write the draft to a temporary file:

```json
{ "repository": "owner/name", "target": { "type": "new", "title": "…", "body": "…" }, "addLabels": ["ready"], "blockedBy": [12], "questions": [] }
```

For a refinement, the target is `{ "type": "refine", "issue": 72, "comment": "…" }`
and `removeLabels` is allowed.

`kitchn issue new --draft <file> --revision <rev> --registry <dir> --json`
`kitchn issue refine <n> --draft <file> --revision <rev> --registry <dir> --json`

Show the person the whole preview: every issue, comment, label, and
dependency, and the digest. A preview with open questions is not ready.
Nothing is posted by these commands. Once the person approves that digest,
post it with the same draft file:

`kitchn issue apply --draft <file> --approve <digest> --registry <dir> --holder <you>`

It writes through the house's forge binding, and only the exact preview the
digest names; a changed draft is refused until the person approves its new
digest. When it stops early (a refused or unknown write), rerun the same
command: it resumes without duplicates. A house without a forge binding is
refused; tell the person to run `kitchn forge bind`. Post nothing yourself.

A draft refused because an earlier draft settled after writing
(`EarlierSettledWithWrites`) stays refused until the person releases it.
Show them the task and its writes, have them check those writes on the
forge, and run with their reason:

`kitchn issue acknowledge <task> --reason <why> --registry <dir> --holder <you>`

With a forge binding it first re-reads the forge for writes whose outcome is
unknown; without one, such a write stays unknown. Add `--accept-unknown`
only after the person has checked it and says to. Never release a subject on
your own.

## 3. End or hand over

When you stop before the work is done, hand the claim back so scheduled runs
or another session can adopt it:

`kitchn hand-back <task> --registry <dir> --holder <you>`

## 4. Scheduling

A request such as "schedule pickup every 15 minutes" needs a preview
(repository, interval, agent), the person's approval, and `kitchn house
doctor` reporting every capability scheduled pickup needs as observed. This
build has no schedule command: say so and create nothing. Scheduled runs act
on house grants, never on this session's approvals.

## Never

- Act on a house you guessed, or without the pinned instructions.
- Post, push, label, launch, or schedule without the person's approval of
  that exact action.
- Work on an item another trigger holds, or take over a live claim.
- Report fan-out, checks, or reviews you did not observe.
