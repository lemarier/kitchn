# Initialize or adopt a repository

Register a reviewed house configuration with `kitchen house init` and import its
pinned guidance with `kitchen house sync` first. The house allowlist must contain
the repository. `--template` names a template in that guidance: Kitchen verifies
the house's immutable snapshot for its configured guidance revision, loads
`templates/<name>/` from the verified bundle, and records that revision in
provenance markers. A missing or modified snapshot, or a name absent from the
pinned guidance, is refused. Authenticating the bundle's origin remains the
caller's step before `house sync`. Templates are trusted code inputs and must
terminate.

```sh
kitchen init my-project --registry /path/to/registry \
  --house example --repository example/my-project \
  --template example --set project_name=my-project
```

Supply each variable once as `--set name=value`; the value is everything after
the first `=`. A missing-value error lists every required variable with its
description.

The default is a preview of every addition, identical file, and conflict. Repeat
with `--confirm` to answer `yes`, or `--yes` for non-interactive confirmation.
EOF, a negative answer, or an incomplete answer grants no consent. Invalid input
exits 2; conflicts and execution failures exit 1. Nothing creates a Git remote,
pushes, starts schedules, or accesses credentials. `init` creates the repository
directory and files, without running `git init` or activating workflow files.

`init` accepts a missing or empty directory and refuses one with content. For an
existing directory, use `kitchen adopt` with the same arguments. Once
`.kitchen.json` exists, omit `--house` and `--repository` to use that exact
binding. Missing selection fails closed, even with only one registered house.
Existing workflow selections and additional checks/reviewers are retained;
new bindings select no workflows. Templates cannot replace the binding.

After `kitchen house update` selects a newer guidance revision, rerun `adopt` to
preview its template updates; earlier snapshots are retained for active tasks
but are not used for new plans. Provenance
markers distinguish pristine managed files whose upstream content or revision
changed from files edited locally. Both are conflicts and remain untouched,
even with `--yes`; reconcile them manually. Only missing files are installed.
Unchanged reruns do nothing. An interrupted create-only install can be rerun;
inspect any reported partial-installation failure before proceeding.

The CLI rejects a symlink or file as the target itself, including a dangling
link. It resolves symlinked ancestors (such as macOS `/var`) once. Links and
redirected paths below that resolved root are still refused by the installer. Parent (`..`) components are rejected; supply the intended root
explicitly.

## House guidance and bootstrap assets

Instruction snapshots and repository bootstrap files are separate. Import a
caller-verified, exact-pin instruction bundle using:

```sh
kitchen house sync --registry /path/to/registry --house example \
  --bundle /path/to/verified-bundle.json
kitchen house doctor --registry /path/to/registry --repository-path my-project
```

`house sync` installs assets in the external registry's immutable snapshot; it
does not copy `.origin89/` into the consumer. `init` and `adopt` do not execute
scripts, fetch guidance, or automatically import bundles. Doctor reports missing
pinned instructions and unobserved access separately from scaffold success.

A house template that offers a guidance refresh recipe must ship the executable
bootstrap it invokes and that bootstrap's licenses and notices as declared
template files, preserving their bytes. Kitchen's offline checks do not run such
a recipe and do not establish that guidance was installed. For pinned tasks use
the immutable snapshot returned by `house sync`; a refresh script that follows
upstream must not silently change an active task's pin.

Kitchen ships only `example/`. Copy it into your house guidance as
`templates/<name>/`, with `template.toml` and its `files/` tree as bundle assets,
set the house identity and a manifest `name` matching the directory, declare all
files, and bump the template revision when content changes. A template renders
at most 255 files and 8 MiB minus 64 KiB in total, leaving one installer slot and
64 KiB for `.kitchen.json`; an instruction bundle holds at most 200 assets across
all of its guidance and templates.
Keep product policy separate from Kitchen's development standards.

## Values in structured output

Variables default to `kind = "text"`: single-line text, never evaluated as another
template. Houses must select an appropriate constraint before interpolating a
value into TOML, Rust, JSON, or a shell command. Constraints reject unsupported
values; they do not escape or silently rewrite them.

- `quoted-text`: no double quote, backslash, or backtick; suitable inside the
  fixtures' double-quoted strings and Rust doc comments.
- `rust-identifier`: 1–64 lowercase ASCII letters, digits, or underscores,
  starting with a letter; Rust keywords and hyphens are rejected.
- `version`: exactly three decimal u32 components, with no leading zeroes;
  suitable for the fixture's `cargo +<version>` shell token.
- `year`: exactly four ASCII digits.
- `https-url`: an `https://` prefix and a nonempty remainder, using only ASCII
  letters, digits, `/`, `:`, `.`, `_`, and `-`.

All kinds also reject control characters and values over 1 KiB. Defaults receive
the same validation as supplied values. Use context-specific escaping in a
house template if it needs richer text; do not put unrestricted text directly
into structured syntax or commands. For example, the Origin89 fixture rejects
`summary='A "quoted" tool'` rather than producing invalid TOML.

Provenance cannot be enabled for executables or content beginning with `#!`,
`---`, or `+++`. Those formats need their first line; leave them unmarked. The check runs
after rendering, so variables cannot introduce a displaced shebang or front matter.
