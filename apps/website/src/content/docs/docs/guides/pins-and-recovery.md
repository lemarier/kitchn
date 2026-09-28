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
kitchn house update --registry ~/.kitchn --house acme --bundle new-verified-bundle.json
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

## File safety

The installer refuses redirected roots, parents and target paths, and never
rewrites an existing managed link. Each batch is limited to 256 files and 8 MiB.
There are no retries or network calls; a rollback removes only files and empty
directories the same call created and still owns, and reports anything
uncertain.
