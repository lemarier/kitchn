---
title: Pins and recovery
description: How guidance revisions are pinned, updated and recovered.
---

Every task records the exact kitchn revision, house guidance revision and
repository instructions it runs under. Updating a house never changes the rules
of a task already in progress.

## Sync and update

- `kitchn house sync` installs the revisions already configured for the house.
- `kitchn house update` is the only command that changes them, and only after
  every file in the new snapshot verifies.

```sh
kitchn house update --house <id> --bundle new-verified-bundle.json
```

Old snapshots are kept, so active tasks keep resolving the rules they started
with. New plans use the new pins.

## What gets verified

The bundle's `roleCardsDigest` must match the role cards built into your kitchn
binary. kitchn checks the content of the bundle; it does not fetch Git objects
or verify remote signatures, so authenticating the bundle's origin happens
before `sync`. Required notice files must be in the bundle and are kept
byte for byte.

The registry is trusted, owner-controlled state. It is not tamper-evident
against another process running as the same user.

## When something goes wrong

- **A failed update** keeps the previous pins.
- **An interrupted snapshot** can be completed by rerunning the same verified
  bundle, as long as every file already written is complete.
- **A partial or mismatched file** is a conflict. Keep it and inspect it; kitchn
  won't overwrite it.
- **A `.pending` file** means a config update was interrupted. Inspect it and
  the current config, move it aside yourself, then retry.

Nothing is cleaned up automatically. A damaged house configuration is reported
on its own and doesn't stop other houses from resolving.

## Orca coordinator restart

An Orca restart can invalidate the coordinator terminal saved by `kitchn tick
configure`. Run `kitchn house doctor` from the bound repository. Its coordinator
finding names a stale handle and the recovery: create a live terminal with
`orca terminal create --focus`, bind it to the existing Run with `orca
orchestration run-use --id <run> --from <new-terminal>`, then store that
terminal with `kitchn tick configure --registry <registry> --house <house>
--orca-coordinator <new-terminal>`.
Check the Run and its pending deliveries before resuming scheduled passes.
The doctor probe only reads the mailbox; it does not acknowledge messages or
change the Run binding.

If the worker's report was lost during the restart, coordination can settle
the task after its linked PR merges. It requires the PR's exact head to match
the task's last checked push, a forge merge commit, and a successfully settled
worker. An open PR or a different head still needs the report or a person.

A round created by older code may lack the worktree resource needed to
relaunch. From a separate owner checkout, run `kitchn task cancel <task>
--reason "older round cannot relaunch"` to inspect its effects and workers.
If every effect is resolved and every worker has stopped or settled, repeat
with `--confirm`. Kitchen records a fresh fenced cancellation, allowing the
next follow-up pass to create a round with the current task specification.
The command refuses an uncertain effect or worker and names it; investigate
that outcome before retrying.

## File safety

The installer refuses redirected roots, parents and target paths, and never
rewrites an existing managed link. Each batch is limited to 256 files and 8 MiB.
There are no retries or network calls; a rollback removes only files and empty
directories the same call created and still owns, and reports anything
uncertain.
