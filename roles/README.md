# Kitchen roles and house adoption

The eight cards in this directory are generic responsibility and evidence
contracts. They start no processes and grant no authority. House specializations
and repository instructions are loaded separately from task-pinned revisions.

## Adopt a repository

A house owner supplies a reviewed `HouseConfig` JSON document and a verified
`InstructionBundle` export. Register the policy in a private, owner-controlled
external directory; do not place the registry inside a checkout. Paths below
are examples to replace, not commands that activate a real house.

```sh
kitchen house init --registry /absolute/external/kitchen --config /path/house.json
kitchen house sync --registry /absolute/external/kitchen --house example --bundle /path/verified-bundle.json
kitchen house setup --registry /absolute/external/kitchen --repository owner/project
```

Setup asks for the house and workflows. Choose `none` for interactive-only work.
It finds the nearest Git root, writes only `.kitchen.json` there, and finishes
with a doctor report and next steps. An explicit `--repository-path` can adopt
a directory before Git initialization; implicit setup outside Git is refused.
The binding contains the house and repository identities, selected workflows,
and additional reviewer/check requirements. It cannot contain credentials,
grants, private context, or house-policy overrides. External house policy keeps
`policyLimits` separate from standing `grants`; interactive permission is never
promoted into a scheduled grant. Repository effects use repository-scoped grants
with explicit backend and credential identifiers, and posting grants must name
an allowed posting destination. Existing reviewer/check
additions survive setup reruns. A changed house or repository is a conflict,
not an implicit migration.

Use `--house example --workflows pickup,gate` to provide the two choices without
prompts. `--preview` reports the proposed setup without writing the binding.
`--repository-path /absolute/checkout` targets another repository. `--json`
returns the doctor findings plus `preview`, `binding` (created, unchanged,
updated, conflict, or refused), and `written` fields. Setup does not create
labels or start schedules; label creation is handled by the scoped GitHub
integration after its preview and authorization checks. Missing label inventory
is unknown, not permission to create. Existing conflicting labels are retained.
Exact-name labels with color or description drift are reported informationally and never recolored or renamed.
Disabling a workflow leaves its labels in place.

```sh
kitchen house doctor --registry /absolute/external/kitchen --repository-path /absolute/checkout --json
kitchen house update --registry /absolute/external/kitchen --house example --bundle /path/new-verified-bundle.json
```

Doctor's optional `--evidence /path/scoped-observation.json` accepts a
`DoctorEvidence` record from the integration: exact house and repository,
capabilities, observed labels, and access status. These are diagnostic
observations, not credentials or action authority. No evidence means unknown.
Setup exits 0 when adoption or its preview succeeds, even when doctor lists
remaining work. Doctor exits 0 for complete configuration evidence and 1 when
findings remain. Conflict, refusal, execution or output failure exits 1; invalid
input exits 2.

## Pins and recovery

`sync` uses the configured Kitchen and guidance revisions. Only `update`
changes them, after every file in the new snapshot verifies. A source exporter
must authenticate the bundle and compute `roleCardsDigest` from the claimed
Kitchen revision. Before writing, the importer compares this required SHA-256
digest with `adoption::role_cards_digest()` over its embedded role cards. The
digest covers the domain `kitchen-role-cards-v1` followed by a NUL byte, then
lexicographically ordered `roles/<name>.md` paths and their exact UTF-8 contents;
each path and content is prefixed with its u64 big-endian byte length. The
importer verifies content identity, not the Git identity of the export: it does
not fetch Git objects or verify remote signatures. Builds need no Git checkout.
Required notice paths must be included in the bundle and are kept
verbatim. Snapshots retain their exact manifest and content; a fresh agent
resolves them through `HouseRegistry::resolve`, using the task's repository
instruction commit. Existing tasks keep their `ResolvedInstructions` path and
core `Provenance`; `ResolvedInstructions::verify` rechecks those retained pins
before a fresh agent joins the task. The retained reference also pins the role
digest, independently of later binaries. Updates do not delete old snapshots.
The local registry and manifest remain trusted, owner-controlled state; this
is not a tamper-evident store against the same operating-system user.

A failed update retains the previous pins. An interrupted create-only snapshot
can be completed by rerunning the same verified bundle if every existing file
was fully written. A partial or mismatched file is a conflict; preserve and inspect it instead of overwriting it. A `.pending` file
means a config update was interrupted: inspect it and the current config before
explicitly moving it aside and retrying. No cleanup of these files is automatic.
Pending and stray directory entries
are ignored during house selection; damaged house configurations are reported
individually and do not prevent resolving another house.

The installer rejects redirected roots, parents and requested file paths.
Existing managed links and local skills outside the requested paths are left
alone; Kitchen never rewrites a managed link's target. Trees must be controlled
by the local user, and cooperating callers must serialize writes. Path checks
do not defend against a malicious process running as the same user racing
filesystem operations. Local I/O is bounded by 256 files and 8 MiB per batch;
the manifest repeats the assets, so its bytes count toward that limit too.
There are no retries or network calls. Rollback only removes call-created,
still-owned files and empty directories; uncertain leftovers are reported.

`adoption::RelativePath`, `NewFile`, `FileMode`, `SafeInstaller::preview`, and
`install_new_files` are the reusable create-only API for template consumers.
A conflicting batch returns `HouseError::Conflicts(report)` and writes nothing;
`Ok(report)` means all files were created or already identical. New repository
files use 0644 (regular) or 0755 (executable), subject to umask. Private registry
files use 0600 and directories use 0700. Create-only reruns preserve existing
permissions.
`house::workflow_requirements` and `preview_labels` are the declarations and
read-only label preview for forge integrations. These library entrypoints do
not depend on Orca.
