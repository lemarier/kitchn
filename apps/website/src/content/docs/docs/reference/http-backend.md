---
title: HTTP worker protocol
description: The HTTP protocol a service implements to run a house's workers for kitchn.
---

A house can run its workers on any service that implements this protocol, such
as a hosted sandbox control plane that starts one container per worker. kitchn
is the client. Every call is a bounded, synchronous request; kitchn keeps its
own task state and treats the service as the source of worker facts only.

The protocol covers launching, messaging, replying to, cancelling, and
releasing workers; observing a worker; listing resources; the coordinator
mailbox; and usage reports. It has no schedule calls, so a house on an HTTP
backend cannot run commands that install or pause schedules, such as
`kitchn budget`.

## Binding a house

The house's worker backend binding names the `http` kind, the backend
namespace, the credential, and the endpoint:

```json
"backend": {
  "kind": "http",
  "backend": "sandbox",
  "credential": "sandbox-token",
  "endpoint": "https://sandbox.example.com/kitchen"
}
```

Guided `kitchn house init` only sets up Orca, so this binding is written into
the house policy by hand for now. The endpoint must be `https`, or plain `http`
to `127.0.0.1`, `localhost`, or `[::1]`. Its host must be a DNS name, an IPv4
address, or a bracketed IPv6 address, and any port must be 1 to 65535. It may
not contain user information, a query, or a fragment, so no credential can hide
in it.

The bearer token goes in `<registry>/private/<house>/credentials/<credential>`,
owned by you and mode `600`, the same place a forge token goes. kitchn opens it
without following links, refuses it when it is missing, exposed, or larger than
16 KiB, and sends it only in the `Authorization` header. curl receives it
through a configuration on stdin, never in its arguments.

## Conventions

- Every path is below the endpoint and starts with `/v1/`.
- Every request carries `Authorization: Bearer <token>`. A service answers
  `401` or `403` before doing anything else when the token is wrong.
- Bodies are JSON. Calls that name a worker or a batch use `POST` with a body,
  so handles never need URL encoding.
- Only a `200` response carries an answer. For reads, any other status means
  the call failed and nothing may be inferred. Kitchen does not follow
  redirects, so a `3xx` is such a status.
- A service must never forward the `Authorization` header or any credential to
  a worker. Requests name credentials (`"credential": "sandbox-token"`); they
  never carry one. A service that injects credentials, for example at egress,
  resolves the name itself. Kitchen sends only requests that name the
  binding's credential, the one whose token authenticates the call.

Identifiers (`backend`, `house`, `task`, `credential`) are 1 to 64 ASCII
letters, digits, `-`, or `_`. Handles, keys, and message ids are opaque strings
of at most 256 bytes. Text such as a brief is at most 64 KiB.

## Descriptor

`GET /v1/descriptor` says which backend namespace and house the service serves
and what it supports:

```json
{
  "backend": "sandbox",
  "house": "origin89",
  "capabilities": {
    "worker.launch_isolated": "supported",
    "worker.launch_readiness": "supported",
    "worker.status_and_outcome": "supported",
    "worker.messaging": "supported",
    "worker.cancel": "supported",
    "worker.deliveries": "supported",
    "run.transfer": "supported",
    "resource.inventory": "supported",
    "resource.release": "supported",
    "usage.attribution": "partial",
    "effect.lookup": "supported",
    "effect.idempotent_requests": "supported"
  },
  "workerSelection": {
    "families": ["claude", "codex"],
    "model": true,
    "effort": "with-model"
  }
}
```

kitchn refuses to connect when `backend` or `house` differs from the house's
binding. Support is `supported` or `partial`; only `supported` satisfies a
workflow requirement. kitchn declares the reported capabilities that this
protocol can serve and ignores the rest, so a service reporting
`schedule.manage` still cannot run schedules through it. Those capabilities
are the `worker.*`, `run.transfer`, `resource.inventory`,
`resource.release`, `agent.select_*`, `usage.attribution`,
`house.credentials`, `effect.lookup`, `effect.idempotent_requests`, and the
per-operation `effect.lookup.<operation>` and `effect.idempotent.<operation>`
for the five worker operations. When a command needs a capability the
descriptor lacks, it stops before any effect and names every missing and
partial one.

`workerSelection` says what a launch can honor of an agent selection: the
families it can start, whether it can choose a model, and whether an effort is
`unsupported`, allowed only `with-model`, or `always` allowed. Without it,
kitchn refuses any launch that names a selection, so a worker never runs on
the service's default agent in place of the requested one.

## Effects

`POST /v1/effects` performs one worker operation. The body is the run and the
request kitchn persisted before sending it:

```json
{
  "run": "run-1",
  "request": {
    "house": "origin89",
    "backend": "sandbox",
    "credential": "sandbox-token",
    "task": "issue-195",
    "attempt": 1,
    "key": "k-8d2f",
    "effect": {
      "executor": "worker",
      "effect": {
        "type": "launch-worker",
        "role": "station-cook",
        "workspace": { "type": "isolated" },
        "brief": "Fix the flaky test.",
        "branch": "kitchen/issue-195",
        "agent": { "agent": "codex", "model": "gpt-6-sol" }
      }
    }
  }
}
```

The same key is also sent as the `Idempotency-Key` header. The other
operations are:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `launch-worker` | `role`, `workspace`, `brief`, optional `branch` and `agent` | Start a worker. `workspace` is `{"type": "isolated"}` or `{"type": "existing", "resource": <resource>}`. With a `branch`, create exactly that branch or refuse. |
| `message-worker` | `worker`, `body` | Deliver a message. |
| `reply-to-worker` | `worker`, `question`, `body` | Answer the question with message id `question`. |
| `cancel-worker` | `worker` | Stop the worker. |
| `release-resource` | `resource` | Remove only that resource. Releasing a worker's workspace never deletes its branch. |

A resource is `{"kind": "worker" | "worktree" | "terminal" | "branch" | "schedule", "backend": <namespace>, "handle": <handle>}`.

The service answers `200` with the outcome:

```json
{
  "status": "applied",
  "receipt": {
    "reference": "req-17",
    "created": [
      { "kind": "worker", "backend": "sandbox", "handle": "w-17" },
      { "kind": "branch", "backend": "sandbox", "handle": "kitchen/issue-195" }
    ],
    "touched": []
  }
}
```

`created` lists resources the effect created, which become the task's;
`touched` lists existing ones it acted on. At most 16 resources in total, all
in the service's own namespace.

```json
{ "status": "not-applied", "reason": "rate-limited", "retryAfterSeconds": 30 }
```

`not-applied` is a promise that nothing happened and nothing will. Its
`reason` is `rejected`, `rate-limited` (optional `retryAfterSeconds`),
`unsupported` (optional `capability`), `cross-house`, or `foreign-backend`.

kitchn reads the outcome this way:

| Response | kitchn records |
| --- | --- |
| `200` `applied` | Applied, with the receipt. |
| `200` `not-applied` | Not applied, with the reason. |
| `401`, `403` | Not applied: refused before acting. |
| `429` | Not applied: rate limited. |
| Any other status, an unparsable body, or a receipt naming another namespace's resource | Uncertain: the response was lost. |
| No response within the deadline | Uncertain: timeout. |
| Connection failure | Uncertain: transport. |

An uncertain outcome is reconciled by lookup before any retry. kitchn refuses
some requests itself and sends nothing: a request for another house or
namespace, an operation whose capability is not `supported`, an agent
selection the descriptor does not cover, and a target on another backend.

### Idempotency

A service that declares `effect.idempotent_requests`, or
`effect.idempotent.<operation>`, returns the original receipt when it receives
a key it already applied, without acting again.

### Lookup

`POST /v1/effects/lookup` takes the same body as `/v1/effects` and performs
nothing. It answers `{"status": "applied", "receipt": <receipt>}`,
`{"status": "absent"}`, or `{"status": "unknown"}`. `absent` is a promise that
the key was never applied and that no request still in flight can apply it; a
service that cannot promise that answers `unknown`. Only a service that
declares `effect.lookup`, or `effect.lookup.<operation>`, is asked.

## Workers and resources

`POST /v1/workers/observe` with `{"worker": <resource>}` answers the worker's
state without changing it:

```json
{ "state": "settled", "outcome": "cancelled" }
```

`state` is `starting`, `ready`, `awaiting-reply`, `user-takeover`, `settled`
(with `outcome` `succeeded`, `failed`, or `cancelled`), `missing`, or `unknown`.
Report `ready` only on positive evidence that the agent runs. kitchn reads any
other state as `unknown`, never as readiness. Needs
`worker.status_and_outcome`.

`GET /v1/inventory` lists at most 1024 resources the service manages for the
house:

```json
{
  "resources": [
    {
      "resource": { "kind": "worker", "backend": "sandbox", "handle": "w-17" },
      "owner": "k-8d2f",
      "liveness": "live"
    }
  ]
}
```

`owner` is the idempotency key of the request that created the resource, or
`null`. `liveness` is `live`, `exited`, or `unverifiable`; report
`unverifiable` rather than guessing. Needs `resource.inventory`.

`POST /v1/workers/usage` with `{"worker": <resource>}` answers
`{"status": "none"}` or:

```json
{
  "status": "reported",
  "report": {
    "source": "run-42",
    "agent": "codex",
    "model": "gpt-6-sol",
    "tokens": { "input": 1200, "output": 300 },
    "cost": { "amount": 51000, "basis": "reported" }
  }
}
```

Leave out what you do not know rather than sending zero. `amount` is in
millionths of a US dollar; `basis` is `reported` or `computed`. Needs
`usage.attribution`.

## Coordinator mailbox

Workers report to their coordinator through the service: questions, completion
reports, escalations, heartbeats, and status notes. Each mailbox call names the
run and the coordinator instance:

```json
{ "run": "run-1", "coordinator": "coordinator-1" }
```

| Call | Extra field | Effect |
| --- | --- | --- |
| `POST /v1/deliveries/next` | | Read the oldest unacknowledged batch without consuming it. |
| `POST /v1/deliveries/acknowledge` | `delivery` | Consume the batch with that id if it is the oldest, then read the next. |
| `POST /v1/deliveries/await` | `waitMs` | Like `next`, but hold up to `waitMs` until a batch with a question, report, or escalation waits. |
| `POST /v1/runs/adopt` | | Make this coordinator the run's reader; answers `{}`, or `{ "status": "fenced" }` when another coordinator adopted the run since. Any other body is a failed call. |

Mailbox calls answer one of:

```json
{ "status": "empty" }
{ "status": "fenced" }
{
  "status": "delivery",
  "delivery": {
    "id": "d-4",
    "messages": [
      {
        "id": "msg-3",
        "kind": "question",
        "worker": { "kind": "worker", "backend": "sandbox", "handle": "w-17" },
        "subject": "Which API?",
        "body": "v1 or v2?"
      },
      { "id": "msg-4", "kind": "worker-done", "outcome": "succeeded" }
    ]
  }
}
```

`kind` is `question`, `worker-done`, `escalation`, `heartbeat`, or `status`;
kitchn treats any other kind as needing attention. `outcome` belongs only on
`worker-done`. A row kitchn cannot read is counted as unreadable, so the batch
is never acknowledged blindly; a service may also count rows it could not send
in `unreadable`.

The rules, which kitchn's `run_mailbox` conformance check tests:

- Delivery is at least once: `next` returns the same oldest batch until
  `acknowledge` names it. Messages keep the order workers sent them.
- Acknowledging a batch that is already gone succeeds and consumes nothing.
- Before any adoption, every coordinator of the run reads the mailbox. After
  `adopt`, the adopting coordinator reads the same unacknowledged batch, and
  every call from any other coordinator answers `fenced`. Adoption stops and
  moves no worker.
- `empty` means nothing waits now. It is never evidence that a worker stopped.

Mailbox calls need `worker.deliveries`, and `adopt` needs `run.transfer`.
kitchn holds each wait for at most 15 minutes, split into calls that each fit
one bounded request.

## Testing a service

`kitchen::contracts::conformance` holds the checks kitchn runs against every
backend: `run` and `run_worker` (or `run_worker_on_branch`) for effects and
workers, and `run_mailbox` for the mailbox. The suites perform real effects, so
run them against a live service only where launching and cancelling a worker is
authorized. kitchn's own tests run them against a local fake of this protocol;
that is simulated evidence, not evidence about any hosted runtime.
