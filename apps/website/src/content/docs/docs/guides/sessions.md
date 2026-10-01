---
title: Sessions with /kitchn
description: Work an issue, a pull request or an issue draft from inside your agent session.
---

The `/kitchn` skill makes an agent session act as you in a repository bound to
a house. You hand it an issue, a pull request or a rough idea. It resolves the
house from the repository, follows that house's pinned rules, and asks for your
approval before each external action. Installing the skill grants nothing.

## Install the skill

The skill lives in the kitchn repository at `skills/kitchn/`. Copy or link that
directory into your agent's skills directory, for example
`~/.claude/skills/kitchn` for Claude Code. A `kitchn skill install` command is
planned with the first release
([#18](https://github.com/lemarier/kitchn/issues/18)).

The skill asks for your registry path the first time it needs it.

## What you can hand it

| In your session | What happens |
| --- | --- |
| `/kitchn work #42` | kitchn plans the issue: coordinate its sub-issues, propose a split, implement it here, or stay idle and say why. |
| `/kitchn pr #57` | kitchn plans a review, follow-up, repair or merge-gate pass at the pull request's exact head. |
| `/kitchn issue new` | The session drafts an issue with you and prints a preview to approve. |
| `/kitchn issue refine #61` | The same, for an existing issue. |

You don't type CLI commands. The session runs the matching one (`kitchn work`,
`kitchn pr`, `kitchn issue new`, `kitchn issue refine`) with the facts it read
from the forge, and reads its JSON. The session reads the house rules the plan points to before it does
anything else. Repository instructions such as `AGENTS.md` apply too, but they
cannot relax house rules.

In an unbound repository, the session asks which house and workflows to use,
then runs `kitchn house setup` and shows you the doctor report. It never picks
a house for you.

## Orca and fan-out

When `orca` is on the path, the session captures `orca status --json` and
`orca worktree current --json` and passes them to kitchn. With that evidence,
`work` can propose starting a worker per ready sub-issue, and it asks you
before each one. Without it, the session works as a single agent and says
fan-out is unavailable.

## Claims

`work` and the `pr` writer rounds (`follow-up` and `repair`) take a durable
claim on the issue or pull request, the same claim scheduled runs use, so a session and a scheduled run never work the same
item at once. If someone else holds it, the session tells you who and stops.
A claim lasts 120 minutes by default. The session takes over a claim only when
its lease expired without a hand-back and you ask it to.

When you stop before the work is done, tell the session to hand the claim back.
A scheduled run or another session can then adopt it. Under the hood the
session runs `kitchn hand-back`.

Claims live in the house's state store. `kitchn house init` creates it in the
registry, and sessions find it there.

## Pull request plans

A `pr` plan names the exact head. If the head moves, the plan is void and the
session reads the facts again. `review` and `gate` only read; merging is always
your decision. `follow-up` and `repair` claim the writer round, so scheduled
repair skips the pull request until you hand it back. The house's fix-round
budget applies. You can ask for fewer rounds in a session, never more.

After a supervised writer uses `kitchn push`, Kitchen records whether its
checkout is clean and matches the pushed head. The configured evidence report
file and files ignored by `.gitignore` rules committed at HEAD do not count as
dirty. The worker finishes with the absolute mailbox report command in its
brief. If the earlier writer left no usable
checkout statement, the owner can preview the bound worktree and live PR head
with `kitchn preserve --worktree <launched-worktree>` from a separate
coordinator checkout, then confirm that exact head when the preview is clean.
The owner must type the head prefix on a TTY. On a host where workers share
the OS user and credentials, this checkpoint is not person authentication;
worker isolation such as OpenShell is needed for full enforcement.

Scheduled repair and review follow-up reuse that recorded worktree only while
it still exists, is clean, and is checked out at the live PR head. A removed
worktree needs an owner hand-over because Orca cannot recreate an existing
remote branch checkout. A dirty or moved checkout also stops the round.

When the expediter has reviewed a clean checkout at the live PR head, the short attestation command is `kitchn gate attest --review-id <forge-review-id>`. Kitchen finds the sole open PR at that commit; a dirty checkout, moved head, or ambiguous match is refused.

For supervised work, the house writer GitHub App authors the worker commits
and opens the pull request. The expediter reviews and attests with a separate
GitHub identity, currently a person. A separate review app can fill that role
when a house configures one. The writer cannot approve its own PR; an
attestation from its identity is refused.

## Issue drafts

A preview shows every issue, comment, label and dependency it would write, with
a digest. A preview with open questions is not ready. `issue new` and
`issue refine` post nothing, and this release has no command that posts an
approved draft ([#140](https://github.com/lemarier/kitchn/issues/140)). The
session returns the approved draft for you to post yourself; this release never
posts it.

If an earlier draft on the same subject settled after writing, or possibly
writing, to the forge, new drafts are refused until you release the subject.
The session shows you that task and its writes. Check them on the forge, then
tell the session what you found; it records your reason with
`kitchn issue acknowledge`. A write whose outcome is unknown is released only
after you checked the forge yourself and say so.

## What the session never does

- Act for a house it guessed, or without the pinned rules.
- Post, push, label, launch a worker or create a schedule without your approval
  of that exact action. Approvals never carry over to another action and are
  never saved as a standing grant.
- Take over a live claim.
- Report fan-out, checks or reviews it did not observe.
