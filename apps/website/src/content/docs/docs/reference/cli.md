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

Commands that take `--store` need the house's state store. It must already exist;
no command creates one yet.

## `kitchn house init`

Register a house and pin its guidance. Grants no authority and activates no
workflows.

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
| `--forge-requester <login\|none>` | the logged-in `gh` account, else none. Stores a [forge binding](/docs/concepts/houses/#the-forge-binding), never a credential |
| `--forge-credential <name>` | `github` |
| `--kitchen <commit>` | the commit the binary was built from, when it records one |
| `--bundle <bundle.json>` | kitchn's default guidance, pinned at the kitchn commit |
| `--yes` | ask before registering |

Grants and policy limits stay empty. When standard input is not a terminal,
answers come only from flags and defaults, and any missing answer fails with
exit 2 naming the flags to pass. Rerunning with the same answers changes
nothing; different answers for an existing house are refused.

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
kitchn house doctor --registry <dir> [--repository-path <dir>] [--evidence <file>] [--json]
```

Exits 0 when configuration is complete and 1 when findings remain. Readiness
gaps become findings only for work types whose `mergeReadiness` level is not met.

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

Bind a house to the GitHub account it writes as. kitchn stores no credential:
the token stays in a file you place in the house's private registry directory.

```sh
kitchn forge bind --registry <dir> --house <id> --requester <login> [--credential github] [--posting-budget 20]
kitchn forge show --registry <dir> --house <id>
```

`--posting-budget` caps the writes one task may make (0 to 100). House policy
limits for forge writes must name the credential. `show` exits 1 when the token
file is not ready: missing, not a regular file, reached through a link, owned
by another user, or accessible to group or others.

## `kitchn work`, `kitchn pr` and `kitchn hand-back`

The entrypoints behind the [`/kitchn` skill](/docs/guides/sessions/). Each
prints a plan. `work` and the `pr` writer rounds (`follow-up`, `repair`) take a
durable claim shared with scheduled runs; `review` and `gate` only read.

```sh
kitchn work <issue> --facts <issue.json> --revision <sha> --registry <dir> --store <dir> --holder <you> [--json]
kitchn pr <number> --facts <pr.json> [--as review|follow-up|repair|gate] --revision <sha> --registry <dir> --store <dir> --holder <you> [--json]
kitchn hand-back <task> --registry <dir> --store <dir> --holder <you>
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
kitchn issue acknowledge <task> --reason <text> [--accept-unknown] --registry <dir> --store <dir> --holder <you>
```

`acknowledge` releases the subject of a draft that settled after writing, or
possibly writing, to the forge. Check those writes first. `--accept-unknown`
releases it even when a write's outcome is unknown.

## `kitchn decompose`

Preview a project split into dependency-linked issues. Writes nothing.

```sh
kitchn decompose preview --proposal <proposal.json> [--json]
kitchn decompose acknowledge --store <dir> --house <id> --task <task> --holder <you> --reason <text>
```

`preview` prints a digest and exits 0 when the proposal can be approved, 1 while
ownership overlaps are unordered, and 2 for an invalid proposal such as a
dependency cycle. `acknowledge` releases the repository from an earlier
decomposition that settled without success after writing, or possibly writing,
to the forge. Check the forge for that task's issues first: kitchn cannot
re-read it, so every write not proven applied is recorded as unproven.

## `kitchn cleanup`

Preview what the dishwasher would release, and record your approval.
Releases nothing.

```sh
kitchn cleanup preview --store <dir> --house <id> --inventory <inventory.json> [--remote origin] [--trigger manual] [--json]
kitchn cleanup approve --store <dir> --house <id> --inventory <inventory.json> --holder <you> --digest <sha256:...>
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
kitchn gardener report-stale --registry <dir> --house <id> --store <dir> \
  --repository <owner/name> --issue <n> --body <text> \
  --github-backend <id> --requester <login> --credential <name> --credential-file <path> --gh <path>
```

The repository must be a house posting destination with a comment grant. The
issue is marked handled only after GitHub shows the posted comment. Exits `0`
when recorded or already handled, `1` when the post did not apply (nothing is
recorded; run it again later), `2` for invalid input and `3` when GitHub, the
house or its store refuse or cannot be read. Running it again after a crash
looks the earlier post up instead of posting twice.

## `kitchn budget`

The schedule budget tick: pause schedules that exhausted their usage budget and
report them to the owner.

```sh
kitchn budget precheck --registry <dir> --house <id> --store <dir> --orca <path> --backend <id> --credential <name> --runtime-dir <dir>
kitchn budget run      [same options] [report options]
kitchn budget install  [same options] [report options] --kitchen <path> --cron "15 * * * *" --timezone <tz> --agent claude|codex
```

Report options (`--report-issue owner/repo#n`, `--github-backend`,
`--requester`, `--github-credential`, `--credential-file`, `--gh`) name where
and as whom the owner report is posted. `run` exits 1 when a pause or report
did not go through. `install` adds the tick's schedule paused; activating it is
the owner's separate decision.

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
