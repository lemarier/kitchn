---
title: Houses
description: The organization kitchn works for, and what it keeps separate.
---

A house owns everything that differs between organizations: engineering rules,
which repositories agents may touch, where they may post, which reviewers and
checks a change needs, and which credentials exist. You can run several houses
on one machine, for example an employer, a client and an open-source project.
They never share credentials, private context, answers or history.

## House policy

The policy is a JSON document registered with `kitchn house init`, which can
write it for you from a few answers or register one you wrote and reviewed.

| Field | Meaning |
| --- | --- |
| `house` | House identifier: 1–64 ASCII letters, digits, `-` or `_`, starting with a letter or digit. |
| `kitchen` | The kitchn commit this house runs under. |
| `guidance` | The house guidance commit to pin. |
| `repositories` | Repositories kitchn may work in, as `owner/name`. |
| `postingDestinations` | Where kitchn may post. Posting grants must name one of these. |
| `requiredReviewers` | Reviewers every change needs. |
| `requiredChecks` | Checks every change needs. |
| `grants` | Standing grants with repository or house scope and explicit backend and credential identifiers. |
| `policyLimits` | Limits kept separate from grants. |
| `backend` | The [worker backend](#the-worker-backend) commands build for this house. |
| `mergeReadiness` | Optional. The readiness level (`checked`, `reliable` or `covered`) each work type must reach before the merge gate may merge in a repository with a merge grant. Readiness never grants merge authority. Below the level, a merge needs an owner's Roger approval for that exact pull request, head and base; Kitchen persists the Ask, with the work type, levels and reason, before it is sent. Unobserved readiness counts as `unready`. |
| `graduation` | Optional. Thresholds per work type for moving from supervised to unattended runs: `minSupervisedRuns`, `minFirstPassPercent`, `windowDays`, `onGuidanceChange` (`reset` or `re-evaluate`) and `onRegression` (`report` or `pause-schedule`). Only live runs claimed interactively on the current `guidance` count. Meeting them grants nothing: the owner records an expiring, revocable decision whose grants stay within `policyLimits` and never include merge, publication, schedule activation or equipment. A confirmed regression after the decision suspends it and reports it for review. |
| `tick` | Optional. The workflow passes `kitchn tick` runs: `passes` maps `pickup`, `coordinate`, `repair` or `gate` to `{"everyMinutes": n}`, the least time between two runs' starts. Each interval must be at least `schedules.minIntervalMinutes` when that is set. Absent means the tick runs nothing. |

Permission given during an interactive session is never promoted into a
standing grant.

## The registry

The registry holds house policies and pinned guidance snapshots. Keep it in a
private directory you control, outside any checkout. Private registry files are
created with mode 0600 and directories with 0700.

## The forge binding

`kitchn forge bind` records the GitHub account a house writes as, the
credential name its policy limits refer to, and a posting budget per task. The
token itself stays in a file you place in the house's private registry
directory. kitchn never copies it into its configuration, the repository or a
command line. `kitchn house init` offers the logged-in `gh` account as the
default. A house can instead write as a GitHub App: bind its app ID and
installation, and place the app's private key where the token would go.

## The worker backend

`backend` names the worker backend a house runs on: its `kind` (`orca`, or
`http` for a service implementing the [HTTP worker
protocol](/docs/reference/http-backend/)), the backend namespace its grants
name, and the credential that backend acts under. It holds names, never a
credential value; an `http` binding also holds the service's `endpoint`, and
its token stays in the house's private registry directory. Guided
`kitchn house init` writes `{"kind": "orca", "backend": "orca", "credential":
"orca"}`; it does not set up an HTTP backend yet.

Every command that needs a backend, such as `kitchn budget`, builds it from
this binding and refuses a backend that lacks a capability the workflow
requires, naming each missing one. A house without a binding, or bound to a
kind this kitchn does not know, is refused by name; kitchn never assumes Orca.
A house registered before bindings existed gets one by rerunning
`kitchn house init` with the same answers, or by adding the `backend` field to
its policy.

## The repository binding

kitchn never writes a file into your repository to remember its house. The
binding lives in your registry, keyed by the repository's identity:

```json
{
  "schema": 2,
  "house": "acme",
  "repository": "acme/app",
  "workflows": ["pickup", "gate"],
  "additionalReviewers": [],
  "additionalChecks": []
}
```

A binding names its house and repository, the workflows it opts into, and any
extra reviewers or checks. It cannot contain credentials, grants, private
context or overrides of house policy. A repository can ask for more review,
never less.

## How a repository finds its house

kitchn reads the repository's `owner/name` from one git remote and matches it
against every house's allowlist. That remote is the push destination of the
branch's tracked upstream, or `origin` when the branch tracks none. A fork
checkout whose branch tracks `upstream` is therefore the upstream repository.
Every worktree and subdirectory of the same repository resolves the same way.
If another remote belongs to a different house, kitchn stops and names the
remotes instead of choosing.

- One house claims it: that house is used.
- No house claims it: kitchn stops and says so.
- More than one house claims it: kitchn stops and asks you to choose once. Your
  choice is stored in the registry, never in the repository.

Rerunning setup keeps existing reviewer and check additions. A different house
or repository is reported as a conflict, not migrated silently.

## Older `.kitchen.json` files

Earlier versions wrote `.kitchen.json` into the repository. An explicit,
previewed import moves it into your registry: the preview shows everything that
would be stored, and approving it with the printed digest stores exactly that,
never a file that changed in between. Doctor reports any leftover file
and suggests deleting it; kitchn never deletes files in your repository itself.

## Workflows

A binding can select `pickup`, `triage`, `gate`, `gardener`, `dishwasher` and
`inspector`, or `none` for interactive-only work. Selecting a workflow records
intent; it starts nothing. Each workflow declares the labels and backend
capabilities it needs, and doctor reports which are still missing.
