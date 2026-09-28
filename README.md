# Kitchen

Portable agent workflows by [lemarier](https://github.com/lemarier).

Kitchen will own reusable roles, workflow policy, task contracts, and evidence
records. Each house supplies its own engineering rules and authority. Orca is
the first execution backend; workflow contracts must also support other
orchestrators.

This checkout contains the Rust workspace and development bootstrap. The CLI
supports help, version output, and offline identifier validation. The complete
automation release is tracked in [issue #1](https://github.com/lemarier/kitchen/issues/1); this bootstrap
does not implement or enable those workflows.

## Development

Install Rust through rustup, Python 3.11+, just 1.57+ (CI pins 1.58.0), and
actionlint 1.7.12. The Rust toolchain and initial MSRV are pinned to 1.98.1.
The CLI uses Clap for typed argument parsing. Run `cargo fetch --locked`
before the offline checks.

```sh
just skills-sync
cargo fetch --locked
just check
cargo run -p kitchen-cli -- --help
```

`skills-sync` installs the current engineering guidance into an ignored,
verified local cache. Keep the immutable snapshot it prints for the task.
`just skills-offline` uses that verified cache without checking for updates.
The first refresh needs network access; checks do not refresh instructions.

To install the bootstrap executable locally:

```sh
cargo install --path crates/kitchen-cli --locked --offline
```

The `validate-house` and `validate-task` bootstrap diagnostic commands validate
identifiers without accessing a house, credentials, or a backend:

```sh
kitchen validate-house home
kitchen validate-task task-42
```

Identifiers are case-sensitive, 1–64 ASCII bytes, start with a letter or digit,
and otherwise contain only letters, digits, hyphens or underscores. Validation
preserves spelling and grants no authority. Invalid input exits with code 2;
output failures exit with code 1. The library exposes distinct `HouseId` and
`TaskId` types and structured `kitchen::Error` values.

Read [contributing](CONTRIBUTING.md) and [agent instructions](AGENTS.md) before
working. Library code lives in `crates/kitchen`; the executable lives in
`crates/kitchen-cli`. Exact commands belong in this repository. Work packages and
dependencies live in GitHub issues, not a copied local status inventory.

## Security

Dependabot checks Cargo, GitHub Actions, and the Rust toolchain weekly, with a
seven-day cooldown for version updates. Security fixes are not delayed. Updates
require review; no dependency auto-merge is configured.

The security workflow runs cargo-deny and zizmor on pushes, pull requests, and
a weekly schedule. Dependency review runs on pull requests when the repository
is public; private-repository availability depends on GitHub licensing. For local
networked audits, install cargo-deny and zizmor 1.30.1, then run `just security`
with a repository-scoped `GH_TOKEN` for online action-pin verification. Offline
`just check` does not replace these audits.

See [security reporting](SECURITY.md) for reporting a vulnerability privately.

## License

Kitchen-authored material is copyright lemarier, licensed under
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
The development bootstrap retains its [upstream notices](.origin89/NOTICE.md).
Downloaded skills retain their own licenses. Credentials and private operational
records must never be committed, even while this repository is private.
