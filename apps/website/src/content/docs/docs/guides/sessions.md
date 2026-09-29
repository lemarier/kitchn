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
([#18](https://github.com/lemarier/kitchen/issues/18)).

The skill asks for your registry path the first time it needs it.

## What you can hand it

| In your session | What happens |
| --- | --- |
| `/kitchn work #42` | kitchn plans the issue: coordinate its sub-issues, propose a split, implement it here, or stay idle and say why. |
| `/kitchn pr #57` | kitchn plans a review, follow-up, repair or merge-gate pass at the pull request's exact head. |
| `/kitchn issue new` | The session drafts an issue with you and prints a preview to approve. |
| `/kitchn issue refine #61` | The same, for an existing issue. |

Each of these runs the matching CLI command (`kitchn work`, `kitchn pr`,
`kitchn issue new`, `kitchn issue refine`) with the facts the session read from
the forge. The session reads the house rules the plan points to before it does
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

`work` and `pr` take a durable claim on the issue or pull request, the same
claim scheduled runs use, so a session and a scheduled run never work the same
item at once. If someone else holds it, the session tells you who and stops.
A claim lasts 120 minutes unless you pass `--lease-minutes`. `--take-over` takes
a claim only after its lease expired without a hand-back.

When you stop before the work is done, give the claim back so a scheduled run
or another session can adopt it:

```sh
kitchn hand-back <task> --registry ~/.kitchn --store <store> --holder <you>
```

Claims live in the house's state store, passed as `--store`. The store must
already exist; kitchn has no command that creates one yet.

## Pull request plans

A `pr` plan names the exact head. If the head moves, the plan is void and the
session reads the facts again. `review` and `gate` only read; merging is always
your decision. `follow-up` and `repair` claim the writer round, so scheduled
repair skips the pull request until you hand it back. The house's fix-round
budget applies, and `--fix-rounds` can only lower it.

## Issue drafts

A preview shows every issue, comment, label and dependency it would write, with
a digest. A preview with open questions is not ready. `issue new` and
`issue refine` post nothing, and this release has no command that posts an
approved draft ([#140](https://github.com/lemarier/kitchen/issues/140)). The
session tells you the approved text is ready and posts nothing unless you
approve that exact post.

If an earlier draft on the same subject settled after writing, or possibly
writing, to the forge, new drafts are refused until you check those writes and
release the subject:

```sh
kitchn issue acknowledge <task> --reason "<what you found>" --registry ~/.kitchn --store <store> --holder <you>
```

`--accept-unknown` releases it even when a write's outcome is unknown. Use it
only after you checked the forge yourself.

## What the session never does

- Act for a house it guessed, or without the pinned rules.
- Post, push, label, launch a worker or create a schedule without your approval
  of that exact action. Approvals never carry over to another action and are
  never saved as a standing grant.
- Take over a live claim.
- Report fan-out, checks or reviews it did not observe.
