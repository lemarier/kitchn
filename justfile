default:
    @just --list

skills-sync:
    python3 .origin89/sync-engineering.py

skills-offline:
    python3 .origin89/sync-engineering.py --offline

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --locked --offline -- -D warnings

test:
    cargo test --workspace --locked --offline

# Temporary checks for the upstream Python bootstrap only.
bootstrap-test:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s .origin89/tests -v

build:
    cargo build --workspace --locked --offline

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline

workflow-check:
    actionlint .github/workflows/*.yml

# Keep this compiler aligned with workspace.package.rust-version.
msrv-check:
    cargo +1.98.1 check --workspace --all-targets --locked --offline

check: fmt-check lint test bootstrap-test build docs msrv-check workflow-check

# Website at apps/website (Node 22.12+, pnpm). Kept out of `check` so Rust
# work doesn't need Node; CI runs it as its own job.
website-dev:
    pnpm --filter website run dev

website-check:
    pnpm --filter website run check

# Publishes getkitchn.com. Needs Cloudflare access; never part of checks.
website-deploy:
    pnpm --filter website run deploy

# Networked advisory and workflow audits; kept separate from offline checks.
security:
    cargo deny check
    zizmor --min-severity medium .github/
