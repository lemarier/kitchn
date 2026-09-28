# House template fixtures

These directories are house-owned example content used only by the scaffold
tests. They are not Kitchen defaults and are never installed by Kitchen.

- `origin89/rust-workspace`: the Origin89 house's repository template, derived
  from this repository's layout and the Origin89 engineering conventions. The
  drift test in `tests/scaffold.rs` compares it with this repository.
- `crabnebula/tauri-app`: a synthetic second house with its own Tauri and specta
  conventions. It proves that one house's rules and names do not leak into
  another house's repository. It is not CrabNebula's real policy.

The Origin89 fixture ships the unchanged upstream `.origin89/sync-engineering.py`
with its MIT and Apache-2.0 licenses and a consumer notice, so its `skills-sync`
and `skills-offline` recipes work in a generated repository. The script needs
Python 3 and network access for its first refresh; `skills-offline` needs a
previously verified cache and otherwise exits with an error, which
`generated_origin89_bootstrap_reports_a_missing_cache_offline` checks. Offline
`just check` does not run either recipe or establish that guidance was
installed. Refresh the three upstream files from `origin89hq/engineering`
(`templates/agents/sync-engineering.py`, `LICENSE-MIT`, `LICENSE-APACHE`) when
Kitchen's own `.origin89/` copies change; the drift test compares them.

The Origin89 fixture's generated `Cargo.toml`, `Cargo.lock`, crate manifest,
`src/lib.rs`, and `justfile` intentionally differ from Kitchen's production
files. `DOCUMENTED_DIFFERENCES` in `tests/scaffold.rs` documents those structural
differences; it does not exempt the generated files from validation.
`generated_origin89_fixture_passes_offline_checks` renders the current checkout,
applies every file, and runs the consumer's own `just check`, including its
lockfile, doctest, MSRV and workflows. The test prints a PASS or explicit SKIP
when just/cargo are unavailable; missing actionlint or Rust toolchains fail the
consumer check. A 180-second deadline and 2 MiB log limit bound the check.
