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

The policy is a reviewed JSON document registered with `kitchn house init`.

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

Permission given during an interactive session is never promoted into a
standing grant.

## The registry

The registry holds house policies and pinned guidance snapshots. Keep it in a
private directory you control, outside any checkout. Private registry files are
created with mode 0600 and directories with 0700.

## The repository binding

`kitchn house setup` writes `.kitchen.json` at the repository's Git root:

```json
{
  "schema": 1,
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

Rerunning setup keeps existing reviewer and check additions. A different house
or repository is reported as a conflict, not migrated silently.

## Workflows

A binding can select `pickup`, `triage`, `gate`, `gardener`, `dishwasher` and
`inspector`, or `none` for interactive-only work. Selecting a workflow records
intent; it starts nothing. Each workflow declares the labels and backend
capabilities it needs, and doctor reports which are still missing.
