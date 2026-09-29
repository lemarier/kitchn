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
while ownership overlaps are unordered.

Commands that take `--store` default to the house state store that
`house init` created at `<registry>/private/<house>/store`, located through
`--registry`. Pass `--store` to use another initialized store instead. A store
is never created on the way; a missing one fails.

## `kitchn house init`

Register a house, pin its guidance, and create its state store. Grants no
authority and activates no workflows.

```sh
kitchn house init [options]
kitchn house init --registry <dir> --config <house.json>
```

Without `--config`, it asks only for what it can't infer, prints the resulting
policy, and registers it after you confirm. Press Enter to take the default in
brackets. Each question has a flag:

| Flag | Default |
| --- | --- |
| `--registry <dir>` | `~/.kitchn` |
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
kitchn house sync --registry <dir> --house <id> --bundle <bundle.json>
```

## `kitchn house update`

Change the pins, after the complete new bundle verifies.

```sh
kitchn house update --registry <dir> --house <id> --bundle <bundle.json>
```

## `kitchn house setup`

Adopt a repository by recording its binding in the registry. Writes nothing into
the repository.

```sh
kitchn house setup --registry <dir> [options]
```

| Option | Description |
| --- | --- |
| `--repository <owner/name>` | Repository identity. Read from the checkout's git remote when omitted; naming it skips the remote check, and the house allowlist still applies. |
| `--house <id>` | House to use. Needed only when more than one house claims the repository; the choice is remembered in the registry. |
| `--workflows <list>` | Comma-separated workflows, or `none`. Prompted when omitted. |
| `--repository-path <dir>` | Checkout whose git remotes identify the repository (default: the current directory). Needs a GitHub remote. |
| `--preview` | Report the proposed setup without writing. |
| `--evidence <file>` | Scoped, read-only observations from an integration. |
| `--json` | Print the doctor report plus `preview`, `binding` and `written`. |

## `kitchn house import`

Copy a legacy `.kitchen.json` into the registry. The preview lists the house,
repository, workflows, reviewers and checks it would store, and prints a digest.
Approving needs that digest; if the file changed since the preview, nothing is
stored. The file is never modified or deleted.

```sh
kitchn house import --registry <dir> [--repository-path <dir>] [--json]
kitchn house import --registry <dir> [--repository-path <dir>] --yes --digest <sha256>
```

## `kitchn house doctor`

Diagnose pins, scoped access, labels and backend capabilities, and report
merge readiness: required checks, their recent pass, fail and flaky history,
repository instructions, and work types without an acceptance check. Facts
missing from the evidence are reported as unknown, never as passing. When branch protection binds a check to a GitHub App
(`requiredCheckApps`), only that app's runs count toward the check's history.

```sh
kitchn house doctor --registry <dir> [--repository-path <dir>] [--evidence <file>] [--store <dir>] [--json]
```

Exits 0 when configuration is complete and 1 when findings remain. Readiness
gaps become findings only for work types whose `mergeReadiness` level is not met.
With `--store`, doctor reads the house store and reports any table at 80% of its
limit or more, before new work is refused.

## `kitchn init` and `kitchn adopt`

Preview, then create or add files from a house template.

```sh
kitchn init <dir> --registry <dir> --template <name> [options]
kitchn adopt <dir> --registry <dir> --template <name> [options]
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
kitchn forge bind --registry <dir> --house <id> --requester <login> [--credential github] [--posting-budget 20]
kitchn forge bind --registry <dir> --house <id> --requester <app-slug>[bot] --app-id <id> --installation <id>
kitchn forge show --registry <dir> --house <id>
```

With `--app-id` and `--installation`, the file holds the app's `.pem` private
key. Each write then runs with an installation token limited to its repository
and the permissions it needs, minted with `curl` and refreshed before it
expires. A write to a repository the installation does not cover is refused
before anything is written, naming the repository.

`--posting-budget` caps the writes one task may make (0 to 100). House policy
limits for forge writes must name the credential. `show` exits 1 when the token
file is not ready: missing, not a regular file, reached through a link, owned
by another user, or accessible to group or others.

## `kitchn work`, `kitchn pr` and `kitchn hand-back`

The entrypoints behind the [`/kitchn` skill](/docs/guides/sessions/). Each
prints a plan. `work` and the `pr` writer rounds (`follow-up`, `repair`) take a
durable claim shared with scheduled runs; `review` and `gate` only read.

```sh
kitchn work <issue> --facts <issue.json> --revision <sha> --registry <dir> [--store <dir>] --holder <you> [--json]
kitchn pr <number> --facts <pr.json> [--as review|follow-up|repair|gate] --revision <sha> --registry <dir> [--store <dir>] --holder <you> [--json]
kitchn hand-back <task> --registry <dir> [--store <dir>] --holder <you>
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

## `kitchn issue`

Preview an issue draft. Posts nothing.

```sh
kitchn issue new --draft <draft.json> --revision <sha> --registry <dir> [--json]
kitchn issue refine <issue> --draft <draft.json> --revision <sha> --registry <dir> [--json]
kitchn issue acknowledge <task> --reason <text> [--accept-unknown] --registry <dir> [--store <dir>] --holder <you>
```

`acknowledge` releases the subject of a draft that settled after writing, or
possibly writing, to the forge. Check those writes first. `--accept-unknown`
releases it even when a write's outcome is unknown.

## `kitchn decompose`

Preview a project split into dependency-linked issues, then write the approved
preview to the forge.

```sh
kitchn decompose preview --proposal <proposal.json> [--json]
kitchn decompose apply --proposal <proposal.json> --approve <sha256:...> --registry <dir> --house <id> [--store <dir>] --holder <you> [--json]
kitchn decompose acknowledge (--registry <dir> | --store <dir>) --house <id> --task <task> --holder <you> --reason <text> [--without-forge] [--accept-unknown] [--json]
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
kitchn cleanup preview (--registry <dir> | --store <dir>) --house <id> --inventory <inventory.json> [--remote origin] [--trigger manual] [--json]
kitchn cleanup approve (--registry <dir> | --store <dir>) --house <id> --inventory <inventory.json> --holder <you> --digest <sha256:...>
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
kitchn gardener report-stale --registry <dir> --house <id> [--store <dir>] \
  --repository <owner/name> --issue <n> --body <text> \
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
kitchn store capacity --house <id> (--registry <dir> | --store <dir>) [--json]
kitchn store retain   --house <id> (--registry <dir> | --store <dir>) [--gh <path>] [--window-days 31] [--max-lookups 200] [--apply] [--json]
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
kitchn trust capacity --house <id> --ledger <dir> [--json]
kitchn trust archive  --house <id> --ledger <dir> [--apply] [--json]
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
kitchn audit --registry <dir> --house <id> [--store <dir>] --ledger <dir> \
  (--orca <path> --runtime-dir <dir> | --schedule-evidence <file>) \
  [--open-proposal <key>... | --no-open-proposals] [--json]
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
house's bound forge into one of its repositories, or a schedule consumer.
Other sources, finding text, transcripts, and backend handles stay in the
house.

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
