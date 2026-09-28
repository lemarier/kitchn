---
title: Authority
description: What kitchn may do, and what always needs an explicit grant.
---

kitchn separates doing work from being allowed to act on it. Installing kitchn,
selecting a workflow or opening a role grants nothing.

## Before any external effect

Before kitchn posts, pushes, merges or cleans up, these must all agree:

- the house identity,
- the credential and the house it belongs to,
- the posting destination, which must be on the house allowlist,
- the task and who owns it,
- the exact revision the approval was given for.

If any of them is missing or ambiguous, the action is refused. Missing evidence
and unsupported backend capabilities count as failures, not success.

## Approvals are tied to a revision

An approval covers one exact revision. When a pull request head moves, earlier
approvals no longer apply and the expediter checks again. A green build or two
models agreeing does not replace a required reviewer.

## Trust is earned per station

Autonomy is granted per station, project and kind of work, backed by recorded
outcomes. It is never promoted silently, and trust earned in one house is never
authority in another. Merge, publication and equipment authority are always
separate grants.

## Cleanup

Cleanup needs positive evidence that a resource is owned and finished,
rechecked right before acting. Age or silence only triggers an inspection.
Dirty or untracked work, active workers, pending decisions and unpreserved
commits are kept.
