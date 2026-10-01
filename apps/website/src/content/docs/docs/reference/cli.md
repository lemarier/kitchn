---
title: CLI reference
description: Every kitchn command, its flags and exit codes.
---

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. `setup` also returns 0 when doctor still lists remaining work. |
| `1` | Conflict, refusal, execution or output failure, or doctor findings remain. |
| `2` | Invalid input. |

Scheduled prechecks (`gardener precheck`, `budget precheck`) use their own
codes: `0` when there is work to do, `1` when there is none, `2` for invalid
input and `3` when their inputs cannot be read. `decompose preview` exits `1`
while ownership overlaps are unordered. `kitchn run` exits `3` when another pass
of the same kind holds the lease and `4` when a previous pass's lease expired.

## Defaults

From a bound checkout, most commands accept short forms such as `kitchn tick`
and `kitchn run gate`. An omitted `--registry` comes from `$KITCHN_HOME`, then
`~/.kitchn`. An omitted `--house` comes from the stored binding for the
checkout's GitHub origin repository in that registry. The checkout must have
a readable origin remote whose repository matches the binding; a mismatched
push URL, an unbound checkout, or an ambiguous selection is refused. Pass
`--house` explicitly outside a bound checkout. Explicit flags take precedence.

For commands that consume the store, an omitted `--store` uses
`<registry>/private/<house>/store`. The store stays
house scoped even when several repositories bind to one house. It must already
exist; pass `--store` for a different initialized store. `tick`, `run`, and
`gate` use the house's only repository when there is one. In a house with
several candidates, pass `--repository <owner/name>` or configure the
repository for `tick` and `run`. Other commands may require an explicit
repository. `house init` and `house setup` retain their
own selection flows; `init` and `adopt` still need `--house` for an unbound
target. Scheduled triggers carry explicit registry and house flags because
they can run outside a checkout.

For `gate review`, an omitted `--head` is the clean checkout's `HEAD`, checked against the live pull request head. For `gate review` and `gate attest`, an omitted `--pull-request` is the sole open pull request in the selected house repository at that commit. The checkout repository must match the selected repository; dirty, stale, missing, ambiguous, or incomplete forge evidence is refused. Explicit identifiers override inference. `gate attest` with an explicit PR number reads that PR without requiring a checkout.

## `kitchn house init`

Register a house, pin its guidance, and create its state store. Grants no
authority and activates no workflows.

```sh
kitchn house init [options]
kitchn house init --config <house.json>
```

Without `--config`, it asks only for what it can't infer, prints the resulting
policy, and registers it after you confirm. Press Enter to take the default in
brackets. Each question has a flag:

| Flag | Default |
| --- | --- |
| `--registry <dir>` | `$KITCHN_HOME`, then `~/.kitchn` |
| `--house <id>` | none: always asked |
| `--repositories <owner/name,...>` | the checkout's GitHub remote |
| `--posting-destinations <owner/name,...>` | the repositories |
| `--sous-chef`, `--station-cook`, `--expediter` `<claude\|codex>` | Claude Code at the pass, Codex at the stations |
| `--required-checks <name,...\|none>` | the default branch's required checks, when `--github-requester`, `--github-credential`, `--github-credential-file` and `--gh` can read them; otherwise asked |
| `--required-reviewers <name,...\|none>` | `expediter` |
| `--worker-backend <orca>` | `orca`, the only one supported. Stored as the house's [worker backend](/docs/concepts/houses/#the-worker-backend) |
| `--forge-requester <login\|none>` | the logged-in `gh` account, else none. Stores a [forge binding](/docs/concepts/houses/#the-forge-binding), never a credential |
| `--forge-credential <name>` | `github` |
| `--kitchen <commit>` | the commit the binary was built from, when it records one |
| `--bundle <bundle.json>` | kitchn's default guidance, pinned at the kitchn commit |
| `--yes` | ask before registering |

Grants and policy limits stay empty. When standard input is not a terminal,
answers come only from flags and defaults, and any missing answer fails with
exit 2 naming the flags to pass. Rerunning with the same answers changes
nothing and keeps the house's existing store; different answers for an existing
house are refused. Init refuses a store that belongs to another house, a
redirected store path, and a store inside a Git checkout.

## `kitchn house sync`

Install the house's configured pins from a verified bundle.

```sh
kitchn house sync --house <id> --bundle <bundle.json>
```

## `kitchn house update`

Change the pins, after the complete new bundle verifies.

```sh
kitchn house update --house <id> --bundle <bundle.json>
```

## `kitchn house setup`

Adopt a repository by recording its binding in the registry. Writes nothing into
the repository.

```sh
kitchn house setup --house <id> [--workflows <list|none>] [--repository <owner/name>]
```

| Option | Description |
| --- | --- |
| `--repository <owner/name>` | Repository identity. Read from the checkout's git remote when omitted; naming it skips the remote check, and the house allowlist still applies. |
| `--house <id>` | House to use. Name the chosen house for first setup; the choice is remembered in the registry. |
| `--workflows <list>` | Comma-separated workflows, or `none`. Prompted when omitted. |
| `--repository-path <dir>` | Checkout whose git remotes identify the repository (default: the current directory). Needs a GitHub remote unless `--repository` is given. |
| `--preview` | Report the proposed setup without writing. |
| `--evidence <file>` | Scoped, read-only observations from an integration. |
| `--json` | Print the doctor report plus `preview`, `binding` and `written`. |

## `kitchn house import`

Copy a legacy `.kitchen.json` into the registry. The preview lists the house,
repository, workflows, reviewers and checks it would store, and prints a digest.
Approving needs that digest; if the file changed since the preview, nothing is
stored. The file is never modified or deleted.

```sh
kitchn house import [--repository-path <dir>] [--json]
kitchn house import [--repository-path <dir>] --yes --digest <sha256>
```

## `kitchn house doctor`

Diagnose pins, scoped access, labels and backend capabilities, and report
merge readiness: required checks, their recent pass, fail and flaky history,
repository instructions, and work types without an acceptance check. Facts
missing from the evidence are reported as unknown, never as passing. When branch protection binds a check to a GitHub App
(`requiredCheckApps`), only that app's runs count toward the check's history.

```sh
kitchn house doctor [--repository-path <dir>] [--evidence <file>] [--store <dir>] [--json]
```

Exits 0 when configuration is complete and 1 when findings remain. Readiness
gaps become findings only for work types whose `mergeReadiness` level is not met.
With `--store`, doctor reads the house store and reports any table at 80% of its
limit or more, before new work is refused.
When Orca runtime settings are stored, doctor makes a bounded, read-only mailbox
probe. A stale coordinator handle is named in the findings. Create a live
terminal with `orca terminal create --focus`, bind it with `orca orchestration
run-use --id <run> --from <new-terminal>`, then update the stored handle with
`kitchn tick configure --registry <registry> --house <house> --orca-coordinator <new-terminal>`.
The probe does not acknowledge a delivery or move the Run. Scheduled passes
also check the stored handle before acting and stop with the named recovery
error when it is stale.
For scheduled worker effects, doctor checks grants against the bound
repository and backend. A matching repository-scoped grant satisfies the
check; a house-scoped grant also covers that repository.

## `kitchn house grant` and `kitchn house revoke`

Preview or change standing grants and their policy limits. A workflow selects
its permissions; `--permission` selects one. Use `--repository` to select a
served repository when working outside its checkout. A grant for a selected
repository scopes every permission, including worker messaging, cancellation,
and resource release, to that repository in both sets.

```sh
kitchn house grant --registry <dir> --house <id> --repository <owner/name> --workflow pickup --preview
kitchn house grant --registry <dir> --house <id> --repository <owner/name> --permission message-worker --house-wide --preview
kitchn house revoke --registry <dir> --house <id> --repository <owner/name> --permission message-worker --preview
kitchn house revoke --registry <dir> --house <id> --repository <owner/name> --permission message-worker --house-wide --preview
```

`--preview` writes nothing; `--yes` applies without a prompt. Granting with
`--house-wide` creates house-scoped entries for permissions that support them;
repository-bound forge actions and worker launch remain scoped to the selected
repository. A repository revoke removes matching repository-scoped entries for
that repository. Its
preview names any matching house-scoped entries it retains. `--house-wide`
explicitly revokes matching entries across the house, including grants for
other repositories. Review its preview before applying it.

`house grant --workflow gate` and `house grant --permission merge` refuse to
write a merge grant. To enable the scheduled gate, the owner must configure a
repository-scoped `merge` grant and matching `policyLimits` entry in the house
config, with the forge backend and credential, then set the desired
`mergeReadiness` policy. The gate checks the independent review attestation and
the pull request's exact head, calls `issue_authority`, and resolves that grant
for the exact pull request subject. A guided grant has no subject or attestation.

## `kitchn init` and `kitchn adopt`

Preview, then create or add files from a house template.

```sh
kitchn init <dir> [--house <id>] [--repository <owner/name>] --template <name> [options]
kitchn adopt <dir> --template <name> [options]
```

| Option | Description |
| --- | --- |
| `--house <id>` | Required for an unbound repository; never inferred. |
| `--repository <owner/name>` | Defaults to the repository the target checkout's remotes name. The binding is stored in the registry, never in the target. |
| `--set <name=value>` | Template variable, repeatable. |
| `--confirm` | Print the preview, then ask. |
| `--yes` | Apply without asking. |

## `kitchn forge bind` and `kitchn forge show`

Bind a house to the GitHub account or GitHub App it writes as. kitchn stores
no credential: the token, or the app's private key, stays in a file you place in
the house's private registry directory.

```sh
kitchn forge bind --requester <login> [--credential github] [--posting-budget 20]
kitchn forge bind --requester <app-slug>[bot] --app-id <id> --installation <id>
kitchn forge show
```

With `--app-id` and `--installation`, the file holds the app's `.pem` private
key. Each write then runs with an installation token limited to its repository
and the permissions it needs, minted with `curl` and refreshed before it
expires. A write to a repository the installation does not cover is refused
before anything is written, naming the repository.
For an app binding, `bind` reads `GET /users/<app-slug>[bot]` and stores its
numeric bot user ID with the binding. Worker commits use the app login as
`user.name` and `<bot-user-id>+<app-slug>[bot]@users.noreply.github.com` as
`user.email` in their own Git worktree. A missing or mismatched bot lookup
refuses the bind. A worker launch or push reads GitHub and records the ID in
an older app binding that lacks it; a failed lookup leaves that binding as it
was and refuses the worker action.

`--posting-budget` caps the writes one task may make (0 to 100). House policy
limits for forge writes must name the credential. `show` exits 1 when the token
file is not ready: missing, not a regular file, reached through a link, owned
by another user, or accessible to group or others.

## `kitchn push`

Push the launched worker's task branch and deliver its pull request from that
worktree. Use the exact `Push:` command in the launch brief after committing and
running the required checks.

```sh
kitchn push --store <house-store> --house <id> --task <id> [--acceptance-done]
```

Worker push requires a GitHub App forge binding. Bind the house with
`kitchn forge bind --app-id ... --installation ...` and the app's bot requester;
the app installation must cover the task repository. Kitchen requests an
installation token limited to that repository and contents write. A house
bound to a personal token is refused before that credential is read or passed
to a child process. Before updating the branch, Kitchen also checks that the
branch commits since the launch base recorded outside the worktree name the house writer as both author and
committer. A foreign commit is refused with its SHA; amend it under the
worktree identity with `git commit --amend --reset-author`, or rebase the
branch. Kitchen sets the identity before starting an Orca worker; the worker
must not change it. Kitchen also checks that the
task's push and pull request grants use the bound forge credential and that
the forge scope permits both effects. If either check fails, correct the house
grant or forge binding before retrying. `--acceptance-done` requires the
worker's evidence report to contain `Acceptance: done`; otherwise the pull
request body says `Part of`
the issue. A refused or uncertain push must be reconciled before retrying.
After a checked push, Kitchen compares the live branch tip with the checkout's
HEAD and records clean/pushed facts at that exact head. Untracked files are
ignored only when `.gitignore` rules committed at HEAD ignore them. Local
`.gitignore` edits, `.git/info/exclude`, and global excludes cannot hide work.
The report path named in the task's launch brief is exempt only when it is a
regular file; other changed or untracked paths record `clean no`. Tracked files
with skip-worktree or assume-unchanged flags also prevent a clean result
because they can hide edits from Git status. The command prints both facts. The worker
then uses the brief's absolute `kitchn mailbox report` command as its final
checkout action, stating those facts. Orca's `worker_done` carries no checkout
fields, so coordination retains the checked push observation at that head.

## `kitchn preserve`

Inspect a settled task's bound worktree and the live PR head before a person
confirms its work is preserved. The first call previews dirty files and the
unpushed range; repeat with `--confirm-preserved` only after reviewing it.

```sh
kitchn preserve <task> --pull-request <n> --head <sha> --holder <you> --registry <dir> --worktree <launched-worktree>
kitchn preserve <task> --pull-request <n> --head <sha> --holder <you> --registry <dir> --worktree <launched-worktree> --confirm-preserved
```

Run the command from a separate coordinator checkout. Kitchen matches Orca's
recorded worktree ID and path to the task's launch record. The checkout must
belong to the registry's bound house and repository, match the task's branch
and exact live PR head, and have no changes except files covered by committed
`.gitignore` rules and the configured regular report file.
A different head, dirty file, missing forge observation, or unsettled task is
refused. The recorded decision applies only to that head.
Confirmation refuses a worker environment or launched worktree and requires an
interactive TTY where the owner types the displayed head prefix. This checkpoint
cannot authenticate a person when workers share the same OS user and credentials.
Full enforcement requires worker isolation such as OpenShell, deferred here.

## `kitchn work`, `kitchn pr` and `kitchn hand-back`

The entrypoints behind the [`/kitchn` skill](/docs/guides/sessions/). Each
prints a plan. `work` and the `pr` writer rounds (`follow-up`, `repair`) take a
durable claim shared with scheduled runs; `review` and `gate` only read.

```sh
kitchn work <issue> --facts <issue.json> --revision <sha> --holder <you> [--json]
kitchn pr <number> --facts <pr.json> [--as review|follow-up|repair|gate] --revision <sha> --holder <you> [--json]
kitchn hand-back <task> --holder <you>
```

| Option | Description |
| --- | --- |
| `--facts <file>` | What the session read from the forge: issue status and sub-issues, or pull request state at one head. |
| `--revision <sha>` | The commit whose repository instructions are pinned, such as `git rev-parse HEAD`. |
| `--orca-status <file>`, `--orca-worktree <file>` | Captured `orca status --json` and `orca worktree current --json`. Without them the session works as a single agent. |
| `--repository-path <dir>` | A path inside the checkout (default: the current directory). |
| `--lease-minutes <n>` | Claim lease (default: 120). |
| `--take-over` | Take a claim whose lease expired without a hand-back. |
| `--as <intent>` | `pr` only. Routed from the facts when omitted. |
| `--fix-rounds <n>` | `pr` only. Lowers the house's fix-round budget; it cannot raise it. |

The forge can read review-thread text and locations within configured page and
byte limits. A thread with more than 100 comments is incomplete. Replying to a
thread requires the house's `post-comment` grant; resolving it requires the
separate `resolve-review-thread` grant. Both bind the thread to its pull request
and expected head, persist intent before writing, and read back the result.
Uncertain writes remain pending reconciliation. The scheduled follow-up pass is
not exposed by this CLI yet.

## `kitchn issue`

Preview an issue draft. Posts nothing.

```sh
kitchn issue new --draft <draft.json> --revision <sha> [--json]
kitchn issue refine <issue> --draft <draft.json> --revision <sha> [--json]
kitchn issue acknowledge <task> --reason <text> [--accept-unknown] --holder <you>
```

`acknowledge` releases the subject of a draft that settled after writing, or
possibly writing, to the forge. Check those writes first. `--accept-unknown`
releases it even when a write's outcome is unknown.

## `kitchn decompose`

Preview a project split into dependency-linked issues, then write the approved
preview to the forge.

```sh
kitchn decompose preview --proposal <proposal.json> [--json]
kitchn decompose apply --proposal <proposal.json> --approve <sha256:...> --holder <you> [--json]
kitchn decompose acknowledge --task <task> --holder <you> --reason <text> [--without-forge] [--accept-unknown] [--json]
```

`preview` writes nothing. It prints a digest and exits 0 when the proposal can
be approved, 1 while ownership overlaps are unordered, and 2 for an invalid
proposal such as a dependency cycle. `apply` writes the preview whose digest you
approved, using the house's forge binding. It exits 0 once every write is
applied and 1 when the run stopped early; rerun the same command to resume.
`acknowledge` releases the repository from an earlier decomposition that settled
without success after writing, or possibly writing, to the forge. With
`--registry`, it first re-reads the forge through the house's binding;
`--without-forge` skips that. Check the forge for that task's issues first. A
write still not proven after the forge re-read, including every such write when
the forge is not read, refuses the release unless you pass `--accept-unknown`;
the write is then recorded as unproven.

## `kitchn cleanup`

Preview what the dishwasher would release, and record your approval.
Releases nothing.

```sh
kitchn cleanup preview --inventory <inventory.json> [--remote origin] [--trigger manual] [--json]
kitchn cleanup approve --inventory <inventory.json> --holder <you> --digest <sha256:...>
```

The inventory is a snapshot exported from the backend; Git reads each listed
worktree path. A commit that no `--remote` tracking ref contains is unpushed, so
a worktree holding it is kept. `--trigger` is `manual`, `schedule` or
`disk-pressure`. A scheduled run acts only on steps a person approved by
digest, and only while the evidence still matches. Run `approve` yourself: the
command cannot tell a person from a script.

## `kitchn gardener precheck`

The daily hygiene schedule's precheck. Reads GitHub and the store; writes
nothing.

```sh
kitchn gardener precheck --house <id> --repository <owner/name> --requester <login> \
  --credential <name> --credential-file <path> --gh <path> \
  --ready-label <label> --working-label <label> --lookback-hours <1-168> --stale-days <1-365> [--store <dir>]
```

## `kitchn gardener report-stale`

Post the gardener's stale-issue report and record the issue as handled, so the
daily precheck stays idle for it until someone updates the issue again.

```sh
kitchn gardener report-stale --repository <owner/name> --issue <n> --body <text> \
  --github-backend <id> --requester <login> --credential <name> --credential-file <path> --gh <path>
```

The repository must be a house posting destination with a comment grant. The
issue is marked handled only after GitHub shows the posted comment. Exits `0`
when recorded or already handled, `1` when the post did not apply (nothing is
recorded; run it again later), `2` for invalid input and `3` when GitHub, the
house or its store refuse or cannot be read. Running it again after a crash
looks the earlier post up instead of posting twice. If someone else updated the
issue after the report was decided, the command prints `recorded <url>; later
activity stays unhandled` and the next precheck wakes for that activity.

## `kitchn budget`

The schedule budget tick: pause schedules that exhausted their usage budget and
report them to the owner.

```sh
kitchn budget precheck --registry <dir> --house <id> [--store <dir>] --orca <path> --runtime-dir <dir> [--backend <id>] [--credential <name>]
kitchn budget run      [same options] [report options]
kitchn budget install  [same options] [report options] --kitchen <path> --cron "15 * * * *" --timezone <tz> --agent claude|codex
```

The Orca backend namespace and credential come from the house's
[worker backend](/docs/concepts/houses/#the-worker-backend); a house without
one is refused. `--backend` and `--credential` are optional and refused when
they differ from the binding. Schedules installed earlier still pass them.

Report options (`--report-issue owner/repo#n`, `--github-backend`,
`--requester`, `--github-credential`, `--credential-file`, `--gh`) name where
and as whom the owner report is posted. `run` exits 1 when a pause or report
did not go through. `install` adds the tick's schedule paused; activating it is
the owner's separate decision. The backend must fully support the tick's
required capabilities (no overlapping runs, an enforced run timeout, and typed
idle and error precheck results); otherwise `install` exits 1 naming the missing
and partial ones, and nothing is created. Orca does not provide the first two
and reports precheck errors only in its run history, so the install is refused
there. Activating or trying an installed schedule checks its workflow's
requirements again. On Orca they come from Kitchen's definition of the workflow
the automation's name records; a schedule installed before Kitchen recorded the
workflow, or renamed in Orca to another workflow or consumer, is refused until
it is removed and installed again. Pausing and removing such a schedule still
work.

## `kitchn store`

How full the house store is, and the retention pass that removes markers and
settled tasks no workflow still needs.

```sh
kitchn store capacity [--json]
kitchn store retain [--gh <path>] [--window-days 31] [--max-lookups 200] [--apply] [--json]
```

`capacity` exits 1 when a table is at 80% of its limit or more. `retain` only
previews unless `--apply` is given. With `--registry` and `--gh` it asks the
forge, through the house's forge binding, which issues and pull requests are
closed. Records about an issue or pull request the forge did not answer
completely are kept. A pass looks up at most `--max-lookups` of them; each
applied pass continues after the last one the previous applied pass looked up,
wrapping around, so every item is reached within a few passes. A preview does
not move that position. The output reports how many were not looked up. It never removes asked questions, deliberation threads,
or a task whose write failed and no person has acknowledged. Settled tasks stay
at least 31 days.

The same pass compacts intake. Each repository's settled intake reservations
fold into at most 32 counted-report markers; a reservation whose outcome is not
yet known stays. When the markers are full, the oldest is evicted. Intake then
posts nothing for an uncounted report received at or before the newest evicted
one; it lists the report as late for a person to review. Source times are
capped at the house time a report was reserved, so a source clock running ahead
cannot move that cutoff.

## `kitchn trust`

How full the house trust ledger is, and the archival that moves records no
grant needs out of it.

```sh
kitchn trust capacity --ledger <dir> [--json]
kitchn trust archive --ledger <dir> [--apply] [--json]
```

The ledger holds at most 4096 entries and 8 MiB. When either limit is reached,
new observations and task bindings are refused and earned standing stops
applying to new tasks; revocation still works. `capacity` exits 1 at 80% of
either limit and lists every past archival with its digest.

`archive` only previews unless `--apply` is given. It moves an observation
stream, with every revision and its task's binding, when no grant decision
cites it (proposed, issued, or revoked) and no inspection of it must stay. It
also moves inspections whose deadline has passed and whose samples all have a
result, once their stream leaves too; an inspection of a stream that stays
live, such as one a grant cites, stays with it. Grants, the evidence they cite, and bindings of tasks with no recorded
observation stay, so revoking a grant never needs the archive. The records go
to `archive.jsonl` in the ledger directory, owner-only like the ledger, and the
ledger keeps the batch's SHA-256 digest, counts, and length as one entry.
Bytes past the committed length, left by an archival that failed partway, are
cut off before the next batch, but only when that loses no record: a partial
line, or a batch whose records are all still in the ledger. First, every
committed line is checked against its entry's length and digest. A symlinked,
hard-linked, or shortened archive file, a committed line that does not match,
or an uncommitted batch holding records the ledger lacks is refused without
changing either file. The last happens when `ledger.json` was restored from an
older copy; restore the ledger that committed those batches, or reconcile the
archive file by hand. An archived stream no longer supports new grant proposals.

## `kitchn audit`

Preview the brigade audit of one house: how stations, guidance, and schedules
are performing, and the draft proposals that follow. It reads only; nothing is
filed, and no guidance, grant, or schedule changes.

```sh
kitchn audit --ledger <dir> \
  (--orca <path> --runtime-dir <dir> | --schedule-evidence <file>) \
  [--open-proposal <key>... | --no-open-proposals] [--destination <owner/name>] [--json]
```

The report lists repeated confirmed findings per station and work type,
schedules near their run budget or mostly idle, and stations whose work types'
first-pass acceptance has diverged. Each item states its sample size and lists
up to 20 evidence links, then how many more links and private sources there
are. Simulated deliveries are counted apart and never support a proposal.
Each draft proposes a guidance change, a work type or role split, or a
schedule change, and applying it needs the owner's decision.

Drafts are proposed only when the house budget and the open proposals are both
known; otherwise the report says why and lists the withheld proposal keys.

- Paths: `--ledger`, `--orca`, `--runtime-dir`, and `--schedule-evidence` must
  be absolute; a relative path is refused as invalid input.
- Budget: `--orca` lists every house schedule through the house's bound
  backend, and the run is refused while the house budget is exhausted.
  `--schedule-evidence` is a JSON file of observed schedule runs; it may omit
  schedules, so the budget is unknown and nothing is proposed. The budget is
  also unknown when the observed runs do not reach back to the window's start.
- Open proposals: each draft carries a hidden `kitchn:brigade-audit` marker
  with its key. Read the keys of the open issues and pass every one with
  `--open-proposal`, or pass `--no-open-proposals` when none is open. The
  preview does not read the forge and never assumes that nothing is open.

At most 10 drafts come out of one run; the rest are listed as deferred. The
ledger, store, and schedules must all belong to `--house`. Reports and drafts
hold typed names, counts, and public-safe links only: an `https` link on the
house's bound forge to an issue, pull request, or commit in `--destination`,
the posting destination the drafts will be filed in, or a schedule consumer.
Without `--destination` no forge link is published.
Other sources, finding text, transcripts, and backend handles stay in the
house.

## `kitchn mailbox`

The house mailbox, for workers whose backend carries no worker messages.
Coordination picks it when the worker backend does not declare worker
deliveries, and each worker's brief then gives it the exact commands with its
task and fence.

```sh
kitchn mailbox ask       <scope> --body <text> [--subject <text>] [--wait-secs 0-900]
kitchn mailbox answer    <scope> --question <id> [--wait-secs 0-900]
kitchn mailbox escalate  <scope> --body <text> [--subject <text>]
kitchn mailbox report    <scope> --outcome succeeded|failed [--clean yes|no] [--pushed yes|no] --body <text> [--subject <text>]
kitchn mailbox questions
kitchn mailbox reply --question <id> --body <text> --by person|coordinator
```

`<scope>` is `--task <id> --fence <n>`. A worker posts and reads only for its own task: the fence must
be one its open attempt ran under, so a worker whose attempt ended, or that
names another task, is refused (exit 1), and another task's question reads as
unknown (exit 2). `ask` prints the question id and waits up to `--wait-secs` for
the answer; `answer` reads it later. `reply --by person` also records the
person's time on the asking attempt, and is refused while the task has no
owner, such as during a coordinator handover. A repeated identical reply
changes nothing; a different one exits 1.

`report --clean` states whether the worker's checkout had no uncommitted or
untracked changes other than the configured report path, and `--pushed`
whether its HEAD was the remote branch tip with nothing unpushed. A report
without them records the checkout as unknown unless the exact head has a
checked push observation; otherwise scheduled repair hands the PR over.

A successful scheduled follow-up report uses `--body` with JSON shaped as
`{"sourceHead":"<head at launch>","dispositions":[{"thread":"<thread id>","verdict":"fixed","reply":"<reason>"}]}`.
Use one item for each thread named in the brief; use `declined` when a finding
was not fixed. A change-request review without a thread has no disposition
item. The report's source head comes from the brief, while `--clean yes
--pushed yes` attests to the final checkout after `kitchn push`.

The mailbox holds at most 512 messages, 64 per task, and 32 waiting per task;
a full mailbox refuses the post instead of dropping it. Bodies and answers are
at most 8 KiB. `kitchn store retain` removes handled messages once their
attempt ended or their task settled.

## `kitchn tick`

The one command a trigger runs for a house. launchd, cron, GitHub Actions or a
backend schedule only start it; the house configuration's `tick` passes and
the run ledger in the house store decide what runs.

```sh
kitchn tick
kitchn tick runs
kitchn tick settle --pass <pass> --run <n> --reason <text> --holder <you>
kitchn tick trigger launchd|cron --kitchn <abs path> --registry <abs dir> --house <id> [--every-minutes 1-59]
kitchn tick configure <backend or pickup setting> [--repository <owner/name>]
```

A repair or follow-up pass takes its branch prefix and report path from the pickup
options, and repair, follow-up, and the gate read the house's pinned instructions as
`kitchn run` does.

Each configured pass runs when it never ran or its last run started at least
`everyMinutes` ago. A pass runs under its own lease in the house store, so a
second trigger that fires while it runs prints `busy` and changes nothing. A
pass that runs longer than its one-hour lease renews it, but the lease never
extends past six hours from the run's start. Once the lease lapses or the six
hours pass, the run can no longer record tasks, renew, or record its end.

A run whose lease expired before it recorded an end is recorded as uncertain by
the next tick. Its pass stays blocked: every tick prints `blocked` with the
number of unresolved effects on the tasks the run recorded, and exits 1. A run
may have acted without recording a task, so zero does not mean it did nothing
and no tick clears the block. Check what the run did, then settle it with
`kitchn tick settle`. The ledger records who settled it (`--holder`), when, and
why (`--reason`, at most 4096 bytes). A scheduled trigger cannot settle a run.
Settling a settled run changes nothing.

Each run records its start, end, outcome, usage where the pass reports it, and
up to 8 backend run references as evidence. The ledger keeps the newest 64
settled runs per pass for at most 30 days; it never drops a running or
uncertain run. The tick exits 1 when a pass failed, is blocked, or outlasted its
lease, or when the house configures no passes.

A due pass runs the same scheduled pass as [`kitchn run`](#kitchn-run), in
process, with the same `<backend>` and pickup options; the forge and backend
are resolved only when a pass is due, and only `pickup`, `coordinate`, and
`repair` need the backend. The pass takes its `kitchn run` lease too, so a tick
and a `kitchn run` of the same pass never act at once: whichever starts second
does nothing, and the tick records the run as failed because another run holds
the lease. Before a pass acts on a task it records the task on its run, and it
renews the run whenever it renews its own lease. A pass that cannot start, such
as one without the backend options it needs, is recorded as failed and the
reason is printed. The tick never takes over an expired `kitchn run` lease or
task claim: it records the run as failed until a person checks what the last
run did and runs the pass with `kitchn run <pass> --take-over`. A launched
worker's backend reference is linked as run evidence.

`runs` lists the ledger and reads only. `trigger` prints a launchd plist or a
crontab line that runs the tick every `--every-minutes` (default 5; for cron it
must divide 60, since cron restarts its minute step each hour); it installs
nothing, stores nothing, and changes no live schedule. The printed command names
only the registry and the house.

The host facts a worker backend needs on every tick live in the house's
private runtime configuration, `private/<house>/runtime.json` in the registry:
the Orca executable, runtime directory, Run, coordinator, and repository
selector, the `curl` path for an HTTP backend, and the repository a
multi-repository house's passes serve. Give `configure` the `<backend>` flags, `--repository`, and the pickup settings
(`--ready-label`, `--needs-spec-label`, `--human-label`, `--capacity` from 1 to
64, `--branch-prefix`, `--report-path`, which must stay inside the workspace)
and it stores them there, replacing a valid file and overlaying flags you leave
out on what is stored, then prints whether it stored, updated, or left the file
unchanged. It refuses a call that gives no flag. It validates every argument
before it writes, and it writes through a uniquely named owner-only temporary
that it renames over the file, so a call that fails validation or storage leaves
the file as it was, and a temporary file from another writer is never removed. To
recover from an invalid file, remove it and run `configure` again. `configure` is the only
command that changes the file; `trigger` never writes it and takes none of these
flags. The tick and `kitchn run` read the file. A flag on their command
line that names a different value than the stored one is refused before any
backend is contacted, naming the flag; a flag that repeats the stored value, or
a setting the file does not hold, is accepted. The file holds paths, labels, and
identifiers only, never a credential (unknown fields are refused), and is
created owner-only. A file that others can access, a link, a damaged file, one
for another house, or one with a relative path is refused with an error, and no
backend is contacted. A pass without the
facts it needs is recorded as failed and the error names the flags.

## `kitchn run`

One bounded scheduled pass, for a trigger such as an Orca schedule, launchd, or
cron. `kitchn tick` runs the same passes when they are due. `gate` needs only
the house; `pickup`, `coordinate`, `repair`, and `follow-up` also need the worker backend
options below. Everything else defaults from the house.

```sh
kitchn run pickup
kitchn run coordinate
kitchn run repair
kitchn run follow-up
kitchn run gate
```

These forms use the inferred registry, house, and store, with optional
`--repository <owner/name>` and `--take-over`. The repository defaults to the
house's only one (or its stored runtime choice for these passes). Pickup,
coordinate, repair, and follow-up use the stored backend and pickup settings when
configured; otherwise pass the needed flags explicitly.
The forge is the house's forge binding, with `gh` (and `curl` for a GitHub App)
from `PATH`. `<backend>` holds the host facts of the house's bound worker
backend: for Orca, `--orca <absolute path> --runtime-dir <dir> --orca-run <run>
--orca-coordinator <terminal> --orca-repo <selector>`; for an HTTP backend,
`--curl <absolute path>`. A missing one exits 2 and names what is needed. The
backend must support what the pass requires; otherwise the pass is refused
before anything runs.

- `pickup` claims ready issues of the repository (label, stated acceptance
  criteria, no open blocked-by link, not assigned or reserved), launches their
  workers, and launches the next attempt of a scheduled task whose attempt
  ended. File overlap is not observed, so a pass launches at most one writer
  per repository whatever `--capacity` says, and a new issue waits while
  another scheduled task of the repository is unsettled or any branch writer
  of the repository may be working, a person's `kitchn work` or `kitchn pr`
  session included.
- `coordinate` continues every scheduled task, reads worker deliveries from
  the backend when it declares them and from the house mailbox otherwise, and
  supervises each task once. A worker's successful report settles its task
  with the head its branch shows on the forge. A merged linked PR also settles
  a task whose report was lost when its exact head matches the task's last
  checked push, the forge names a merge commit, and the backend confirms the
  worker settled successfully. The merge commit is recorded as evidence.
  Questions wait for a person
  (`kitchn mailbox reply` on the house mailbox). A delivery stays unread only
  while a message in it waits for a scheduled task the pass does not own yet,
  for a scheduled task's launch whose outcome is not recorded yet, or for a
  report's attempt to end. The pass reconciles such launches on the tasks it
  owns first. Unreadable rows and messages no task can take are acknowledged,
  and each is printed.
- `repair` assesses the open pull requests of settled scheduled tasks and
  prints each decision. For a conflict it launches one repair writer in a new
  checkout of the pushed branch, with a brief quoting the change requests on
  the current head, while the house's fix-round budget lasts. It launches none
  while another branch writer of the repository may be working, scheduled or
  a person's, and at most one per pass. When the branch's last writer did not
  settle successfully with a report of the current head that states its
  checkout clean and pushed (including a checked `kitchn push` observation),
  the pull request is handed over instead. A round whose writer's
  attempt ended without settling it gets its next attempt only through the
  same decision; once that writer ran, it is the branch's last writer, so the
  pull request is handed over. `--branch-prefix` and `--report-path` are
  pickup's: `repair` reads the stored pickup settings and refuses a flag that
  names another value. The `coordinate` pass supervises repair writers like
  pickup workers.
- `follow-up` reads unresolved review threads and change-request reviews on
  open PRs delivered by settled scheduled tasks. It rechecks the head and
  review data before claiming a fix round, shares `followUp.fixRounds` with
  repair, and launches at most one writer in an isolated checkout of the
  pushed branch. A competing repository writer or unproved preservation stops
  the launch. The brief quotes thread IDs, paths, lines, and reviewer text as
  untrusted data. The worker verifies findings, pushes with `kitchn push`
  under the house writer identity, and reports one `fixed` or `declined`
  disposition with a reply for each thread. `coordinate` records all
  dispositions before settling the round, then uses persisted forge effects
  to reply and resolve only fixed threads. Declined threads remain open and
  enter the house mailbox. An exhausted budget asks the house owner there.
  A moved head, changed reviewer comment, incomplete disposition set, or
  uncertain forge write blocks settlement; restarting coordination
  reconciles the recorded effect before continuing. `--branch-prefix` and
  `--report-path` use pickup's stored settings, as for repair.
- `gate` evaluates up to three of those pull requests at their exact heads and
  prints each verdict. It merges one only when an independent reviewer's
  attestation is recorded for exactly its head and base, whoever recorded it
  wrote no part of the branch, the forge shows the
  review it names approved on that head by the claimed login, that login is
  neither the pull request's author nor the author or committer GitHub shows
  for any of its commits, and the house's merge grant covers it. The pull
  request is only reported when GitHub links a commit's author or committer
  to no account, or when it has more than 100 commits. GitHub links a commit
  by the email in it, so this check rules out a reviewer the commits name; it
  does not prove who pushed. A branch a person wrote, through `kitchn work`,
  `kitchn pr`, or a worker's terminal, is only reported too: a session name
  is not a forge login, so the reviewer cannot be told apart from that
  writer. The merge is a squash matched to that head, submitted only
  after the head and base branch are read again. Without an attestation, or
  with any other verdict, it records nothing. A pass that records a merge
  verdict and does not merge, because the head moved or the forge could not
  be read again, continues the same attempt on the next pass, for seven days
  from its first pass. After that the gate task settles as exhausted and the
  pull request is only reported until its head or base changes. When the
  house's pinned revisions or grants changed since, the next pass first
  reconciles and settles the pull request's earlier gate task, then merges
  under a new one. A merge that was sent and whose outcome the forge cannot
  prove blocks every later merge of that pull request, under any gate task
  and whatever risk decision was recorded for it, until a lookup shows it
  merged or shows it can no longer merge.

Each pass takes its own lease first, so a second concurrent start does nothing
and exits `3`. A pass that fails hands its lease to the next start. A pass that
died leaves an expired lease: the next start exits `4` until run with
`--take-over`, which records the takeover. After a `coordinate` takeover, every
scheduled task moves to a new claim before anything else runs, so the replaced
process can no longer act on it; a replaced `pickup` can no longer claim or
launch. The same flag lets `coordinate` take over scheduled task claims that
expired because no pass renewed them for two hours. A pass with nothing to do prints `idle`, exits `0`, and launches or
messages no worker.

## `kitchn gate review`

Post findings through the selected house's forge binding at an exact pull request head. The house must grant `review-pull-request` for the repository and forge credential. The command reads the live base tip, adds a `kitchen-attestation` block, persists the write intent, and reads back the posted review id. An uncertain submission is reconciled by its marker on retry and is never blindly posted again. `--attest` verifies and records an approved review for the scheduled gate; a binding whose login authored the pull request is refused by attestation.

```sh
kitchn gate review --verdict approve --body-file <findings.md> \
  --semantic clean --acceptance complete --hardware complete --risk none [--attest]

kitchn gate review --verdict request-changes --body-file <findings.md>
```

Approval requires all four claim flags. `request-changes` refuses claim flags and `--attest`. The review uses the house's forge identity and its PullRequests write permission; posting a review alone never grants merge authority. A moved head or base refuses submission. The body file must not contain its own attestation block.

## `kitchn gate attest`

An independent reviewer posts an approved forge review on the exact pull request head. The review body must contain one fenced `kitchen-attestation` block. The base SHA must match the live base branch tip; the PR object's base SHA can lag it. Kitchen takes the reviewer login and all claims from that review, checks the PR author, every branch commit author and committer, and the house's launched worker handles, then records the attestation. The scheduled gate reads the same review and claims again before considering a merge.

````md
```kitchen-attestation
head=<40-character commit SHA>
base=<40-character live base tip SHA>
semantic=clean
read_only=true
acceptance=complete
hardware=complete
risk=none
```
````

`semantic` is `clean`, `findings`, `partial`, or `unavailable`. `read_only` is `true` or `false`. `acceptance` and `hardware` are `complete` or `incomplete`. `risk` is `none` or a comma-separated list of distinct classes: `equipment-safety`, `authorization-secrets`, `durable-data`, `public-contract-release`, `workflow-rules`, `dependencies`, `weakened-validation`, and `large-diff`. The block must contain each key once, with no extra keys. Risk classes still require a separate human approval; this command does not grant one.

```sh
kitchn gate attest --review-id <forge-review-id>
```

## `kitchn pickup`

Offline pickup diagnostics.

```sh
kitchn pickup task-id <owner/name> <issue>   # the task id scheduled and interactive work share
kitchn pickup check-branch <name>            # validate a worker branch name without Git
```

## `kitchn validate-house` and `kitchn validate-task`

Check an identifier offline. Identifiers are case-sensitive, 1–64 ASCII bytes,
start with a letter or digit, and otherwise contain letters, digits, `-` or
`_`.

```sh
kitchn validate-house acme
kitchn validate-task task-42
```
