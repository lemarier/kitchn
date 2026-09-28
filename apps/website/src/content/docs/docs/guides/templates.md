---
title: Templates
description: Start or adopt repositories from a house template without overwriting local files.
---

A house can define what its repositories should look like: agent
instructions, a README, ignore files and so on. kitchn renders the template,
shows you every change and adds only what is missing.

## Start a new repository

```sh
kitchn init my-project --registry ~/.kitchn \
  --house acme --repository acme/my-project \
  --template example --set project_name=my-project
```

The first run is a preview:

```text
Template acme/example revision 1, guidance cccc…
Target ./my-project (new repository)
  add        README.md
  add        AGENTS.md
  add        CLAUDE.md
  add        .gitignore
  add        .kitchen.json
5 to add, 0 unchanged, 0 conflicts. Nothing is written until the plan is applied; existing files are never overwritten or deleted.
Preview only; no files changed.
```

Repeat with `--confirm` to be asked, or `--yes` to apply without a prompt. An
empty answer, end of input or anything other than yes grants no consent.

`init` accepts a missing or empty directory. It creates files but does not run
`git init`, create a remote, push or start anything.

## Adopt an existing repository

```sh
kitchn adopt . --registry ~/.kitchn \
  --house acme --repository acme/app \
  --template example --set project_name=app
```

Files that already exist and differ are reported as conflicts and left alone,
even with `--yes`. Once `.kitchen.json` exists, omit `--house` and
`--repository`; kitchn uses the binding.

## Where templates come from

Templates live in the house's pinned guidance under `templates/<name>/`, as a
`template.toml` manifest and a `files/` tree. kitchn loads them from the
verified snapshot for the house's configured guidance revision. A missing or
modified snapshot, or a template name that isn't in the pinned guidance, is
refused.

```toml title="templates/example/template.toml"
schema = 1
name = "example"
house = "acme"
revision = 1
description = "Minimal repository with agent instructions and a README"

[variables.project_name]
description = "Human-readable project name"

[variables.summary]
description = "One-sentence project description"
default = "A new project."

[[files]]
source = "README.md.tera"

[[files]]
source = "AGENTS.md.tera"
provenance = "html-comment"
```

kitchn ships one generic template, `example`. Copy it into your house guidance
and bump `revision` whenever its content changes.

## Variables

Pass each variable once as `--set name=value`. If any are missing, the error
lists all of them with their descriptions.

Variables are plain single-line text by default. Before a value goes into TOML,
Rust, JSON or a shell command, give it a `kind` that constrains it:

| Kind | Accepts |
| --- | --- |
| `quoted-text` | Text without double quotes, backslashes or backticks. |
| `rust-identifier` | 1–64 lowercase letters, digits or underscores, starting with a letter; not a Rust keyword. |
| `version` | Three decimal components with no leading zeroes, such as `1.98.1`. |
| `year` | Four digits. |
| `https-url` | `https://` followed by letters, digits, `/`, `:`, `.`, `_` or `-`. |

Every kind rejects control characters and values over 1 KiB. Constraints reject
values; they never escape or rewrite them.

## Updates and provenance

Managed files carry a provenance marker naming the guidance revision that
produced them. After `kitchn house update` pins newer guidance, rerun `adopt` to
preview the changes. Files whose upstream content changed and files you edited
locally are both reported as conflicts for you to reconcile; kitchn only
installs missing files.

## Limits

A template renders at most 255 files and just under 8 MiB. An instruction
bundle holds at most 200 assets across its guidance and templates.
