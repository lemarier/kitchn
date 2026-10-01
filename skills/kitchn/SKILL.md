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

The CLI binary is `kitchn`. From a bound checkout, it infers the registry
from `KITCHN_HOME` or `~/.kitchn`, the house from the checkout's stored
repository binding, and the house store. Use the short forms below. If the
checkout is unbound or selection is ambiguous, stop and resolve it with the
person. Kitchen writes nothing into the repository's working tree.

For an expediter in a clean PR checkout, use `kitchn gate review --verdict approve --body-file <findings.md> --semantic clean --acceptance complete --hardware complete --risk none` or `kitchn gate attest --review-id <id>`. Kitchen finds the sole open PR for the checkout branch, falling back to the checked out commit when no branch matches, and verifies that commit against the live forge head. The claim flags describe the review and remain explicit. Pass `--head` or `--pull-request` when checkout inference is unavailable.

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
workflows to enable (`none` means interactive only). After they answer, run:

```sh
kitchn house setup --house <house> --workflows <list|none>
```

Add `--repository <owner/name>` when the checkout cannot identify the approved
repository from its Git remotes.

Before Kitchen can launch a writer in an Orca worktree, the repository owner
must explicitly enable Git's per-worktree config once from that checkout.
Preview `kitchn house setup --house <house> --workflows <list|none>
--enable-worktree-config --preview`; it names the repository and the exact
`extensions.worktreeConfig=true` shared Git config change. After the person
approves that change, rerun without `--preview`. This step needs a checkout;
`--repository` alone cannot identify the Git config to change. Launch refuses
with `WorktreeConfigDisabled` and names this setup command while the setting
is off. Each launch then writes identity only to its worktree's
`config.worktree`.

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

For a scheduled repair or follow-up round waiting after an expired claim,
inspect the prior launch, then run `kitchn run coordinate --take-over`.
The writer pass can then adopt the round and launch its next attempt.

### `work <issue>`

Write the issue facts you read from the forge to a temporary file:

```json
{ "status": "open", "subIssues": [{ "number": 15, "status": "open", "blocked": false }], "independentParts": false }
```

`kitchn work <issue> --facts <file> --revision <rev> --holder <you> [orca flags] --json`

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

`kitchn pr <number> --facts <file> [--as review|follow-up|repair|gate] --revision <rev> --holder <you> [orca flags] --json`

Kitchen reads the rounds already spent from the house store and applies the
house's fix-round budget. `--fix-rounds <n>` can only lower that budget.

The plan names the exact head. If the head moves, the plan is void: read the
facts again and rerun. `review` and `gate` are read-only; merging is always
the person's decision. `follow-up` and `repair` claim the writer round, so
scheduled repair skips the pull request until you hand it back.

Review-thread replies use the house forge's `post-comment` grant; resolving a
thread needs `resolve-review-thread`. Verify the exact PR head and the thread
against current code before proposing either effect. A missing or partial forge
read is not permission to retry an uncertain write.

For unattended Kitchen PRs, `kitchn run follow-up` (or a configured
`kitchn tick` follow-up pass) claims a bounded writer round after it reads
unresolved threads or change requests. A follow-up worker uses `kitchn push`
and gives its configured mailbox a JSON report body (the house mailbox uses
`kitchn mailbox report --body`): the launch `sourceHead`
and one `{ "thread", "verdict": "fixed"|"declined", "reply" }` item for
each thread. Kitchen records those dispositions and posts the replies;
fixed threads are resolved, while declined threads go to the house mailbox.

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

`kitchn issue new --draft <file> --revision <rev> --json`
`kitchn issue refine <n> --draft <file> --revision <rev> --json`

Show the person the whole preview: every issue, comment, label, and
dependency, and the digest. A preview with open questions is not ready.
Nothing is posted by these commands. Once the person approves that digest,
post it with the same draft file:

`kitchn issue apply --draft <file> --approve <digest> --holder <you>`

It writes through the house's forge binding, and only the exact preview the
digest names; a changed draft is refused until the person approves its new
digest. When it stops early (a refused or unknown write), rerun the same
command: it resumes without duplicates. A house without a forge binding is
refused; tell the person to run `kitchn forge bind`. Post nothing yourself.

A draft refused because an earlier draft settled after writing
(`EarlierSettledWithWrites`) stays refused until the person releases it.
Show them the task and its writes, have them check those writes on the
forge, and run with their reason:

`kitchn issue acknowledge <task> --reason <why> --holder <you>`

With a forge binding it first re-reads the forge for writes whose outcome is
unknown; without one, such a write stays unknown. Add `--accept-unknown`
only after the person has checked it and says to. Never release a subject on
your own.

## 3. End or hand over

When you stop before the work is done, hand the claim back so scheduled runs
or another session can adopt it:

`kitchn hand-back <task> --holder <you>`

When inspecting cleanup after a stopped launch, include resources from every
attempt. A reconciled `Ended` receipt proves creation and task ownership even
when the dispatch did not finish. Recheck the backend owner, worker settlement,
and worktree preservation for both the stopped launch and its retry before
consenting to a release.

### Worker delivery

When a Kitchen launch brief names a `Push:` command, run that exact command
from the launched worktree after committing and completing the required checks.
Keep the Git author and committer identity Kitchen set for that worktree. The
house writer authors every worker commit and opens the PR; a different GitHub
identity reviews and attests it. The command checks each branch commit's author
and committer, the task's branch, worktree, grants, forge scope, and live remote state, then pushes
with a repository-scoped GitHub App installation token and opens a pull request.
The house must use `kitchn forge bind --app-id ... --installation ...`;
a personal-token binding is refused before its credential is read. Do not call
Git's credential helpers or copy the forge token into a shell command. Add
`--acceptance-done` only after the evidence report contains `Acceptance: done`
and every acceptance item was checked. Otherwise the PR body says `Part of`
the issue. If the command reports a refused or uncertain outcome, stop and
give that result to the coordinator; do not use a direct `git push` to bypass
the check. Later commits use the same command and task branch.
After a successful push, read the printed clean/pushed facts and run the
brief's absolute `kitchn mailbox report` command with those values as the
last checkout action. Kitchen honors committed `.gitignore` rules and exempts
the configured report file when checking cleanliness. Local exclude rules
cannot hide work. If a settled worker left no checkout statement, the
owner can inspect its bound worktree from a separate coordinator checkout with
`kitchn preserve <task> --pull-request <n> --head <sha> --holder <you>
--registry <dir> --worktree <launched-worktree>` and repeat with
`--confirm-preserved` after the preview shows a clean checkout at the live PR
head. Confirmation requires a TTY and the owner must type the displayed head
prefix. It refuses inside a Kitchen-launched worktree or worker environment.
Repair and review follow-up reuse that worktree only when a live check finds
the PR branch at its exact head and no tracked or untracked changes. A missing,
dirty, or moved worktree is handed to the owner; Orca cannot create a fresh
worktree from an existing remote PR branch yet.
On a host where workers share the OS user and credentials, this is an
interactive owner checkpoint, not person authentication: a worker could still
impersonate the owner. Full enforcement requires worker isolation such as
OpenShell; that integration is deferred.

## 4. Scheduling

If `kitchn house doctor` names a stale Orca coordinator handle, create a new
terminal, bind it to the Run with `orca orchestration run-use`, then run
`kitchn tick configure` with the new coordinator handle. Coordination can
settle a lost worker report after the linked PR merges only when the PR head
matches the task's checked push and Orca confirms successful worker settlement.

A request such as "schedule pickup every 15 minutes" needs a preview
(repository, interval, agent), the person's approval, and `kitchn house
doctor` reporting every capability scheduled pickup needs as observed. This
build has no schedule command: say so and create nothing. Scheduled runs act
on house grants, never on this session's approvals.

Before enabling a scheduled workflow, inspect `kitchn house doctor` findings
for that repository. A repository-scoped worker grant can cover its worker
effects. When changing standing authority, preview `kitchn house grant` or
`kitchn house revoke` for the selected repository and get the person's
approval before applying. A selected repository scopes all granted permissions,
including worker messages, cancellation, and resource release. Grant with
`--house-wide` only when the person approves authority across repositories;
repository-bound actions remain scoped to the selected repository. Repository
revoke keeps matching house-scoped grants and limits and names them in the
preview. `--house-wide` revokes
matching authority across all repositories; use it only after the person
approves that broader preview.

Do not use `house grant --workflow gate` or `--permission merge`: the command
refuses both. The owner must configure a repository-scoped merge grant, its
matching policy limit, and merge readiness in the house config. The scheduled
gate verifies an independent review attestation at the exact pull request head,
calls `issue_authority`, and resolves its merge grant for that subject.

## Never

- Act on a house you guessed, or without the pinned instructions.
- Post, push, label, launch, or schedule without the person's approval of
  that exact action.
- Work on an item another trigger holds, or take over a live claim.
- Report fan-out, checks, or reviews you did not observe.
