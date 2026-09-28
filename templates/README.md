# Initialize or adopt a repository

Register a reviewed house configuration with `kitchen house init` first. The
house allowlist must contain the repository. Obtain the template directory from
the selected house's pinned guidance export; Kitchen checks its declared house
and records the configured guidance revision, but does not authenticate the
export's origin. Templates are trusted code inputs and must terminate.

```sh
kitchen init my-project --registry /path/to/registry \
  --house example --repository example/my-project \
  --template /path/to/guidance/templates/example --set project_name=my-project
```

The default is a preview of every addition, identical file, and conflict. Repeat
with `--confirm` to answer `yes`, or `--yes` for non-interactive confirmation.
EOF, a negative answer, or an incomplete answer grants no consent. Invalid input
exits 2; conflicts and execution failures exit 1. Nothing creates a Git remote,
pushes, starts schedules, or accesses credentials. `init` creates the repository
directory and files, without running `git init` or activating workflow files.

For an existing directory, use `kitchen adopt` with the same arguments. Once
`.kitchen.json` exists, omit `--house` and `--repository` to use that exact
binding. Missing selection fails closed, even with only one registered house.
Existing workflow selections and additional checks/reviewers are retained;
new bindings select no workflows. Templates cannot replace the binding.

Rerun `adopt` with a newer reviewed template to preview updates. Provenance
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

A house template that offers a `skills-sync` recipe must supply its executable
bootstrap and notices as declared template files, or instruct the operator to
install them from the same reviewed guidance export before invoking the recipe.
Specifically, the test-only Origin89 Rust fixture requires
`.origin89/sync-engineering.py`, `.origin89/NOTICE.md`, `.origin89/LICENSE-MIT`,
and `.origin89/LICENSE-APACHE` from the reviewed Origin89 bootstrap export.
Preserve their bytes and notices; the script needs Python 3.11+ and network access
for its first refresh. `skills-offline` requires a previously verified cache.
The fixture intentionally omits these upstream assets and is not a standalone
guidance installer. Its offline `just check` does not run `skills-sync` and does
not establish that guidance was installed. For pinned tasks use the immutable
snapshot returned by `house sync`; the legacy refresh script follows upstream
and must not silently change an active task's pin.

Kitchen ships only `example/`. Copy it into your house guidance, set the house
identity, declare all files, and bump the template revision when content changes.
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

Provenance cannot be enabled for executables or content beginning with `#!` or
`---`. Those formats need their first line; leave them unmarked. The check runs
after rendering, so variables cannot introduce a displaced shebang or front matter.
