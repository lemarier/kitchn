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
| `--house <id>` | Required for an unbound repository. |
| `--repository <owner/name>` | Required for an unbound repository. |
| `--set <name=value>` | Template variable, repeatable. |
| `--confirm` | Print the preview, then ask. |
| `--yes` | Apply without asking. |

## `kitchn validate-house` and `kitchn validate-task`

Check an identifier offline. Identifiers are case-sensitive, 1–64 ASCII bytes,
start with a letter or digit, and otherwise contain letters, digits, `-` or
`_`.

```sh
kitchn validate-house acme
kitchn validate-task task-42
```
