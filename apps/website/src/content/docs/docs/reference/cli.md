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

Register a reviewed house policy. Grants no authority.

```sh
kitchn house init --registry <dir> --config <house.json>
```

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
| `--repository <owner/name>` | Repository identity. Read from the git remote when omitted. |
| `--house <id>` | House to use. Needed only when more than one house claims the repository; the choice is remembered in the registry. |
| `--workflows <list>` | Comma-separated workflows, or `none`. Prompted when omitted. |
| `--repository-path <dir>` | Adopt another directory, including one without Git yet. |
| `--preview` | Report the proposed setup without writing. |
| `--evidence <file>` | Scoped, read-only observations from an integration. |
| `--json` | Print the doctor report plus `preview`, `binding` and `written`. |

## `kitchn house doctor`

Diagnose pins, scoped access, labels and backend capabilities.

```sh
kitchn house doctor --registry <dir> [--repository-path <dir>] [--evidence <file>] [--json]
```

Exits 0 when configuration is complete and 1 when findings remain.

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
