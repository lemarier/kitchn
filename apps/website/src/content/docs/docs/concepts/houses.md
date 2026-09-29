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
| `grants` | Standing, repository-scoped grants with explicit backend and credential identifiers. |
| `policyLimits` | Limits kept separate from grants. |
| `mergeReadiness` | Optional. The readiness level (`checked`, `reliable` or `covered`) each work type must reach before the merge gate may merge in a repository with a merge grant. Readiness never grants merge authority. Below the level, a merge needs an owner's Roger approval for that exact pull request, head and base; Kitchen persists the Ask, with the work type, levels and reason, before it is sent. Unobserved readiness counts as `unready`. |

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
