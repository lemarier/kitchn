---
title: Install
description: Install the kitchn CLI.
---

## Install script (macOS and Linux)

Available with the first release ([#18](https://github.com/lemarier/kitchen/issues/18)).

```sh
curl -fsSL https://getkitchn.com/install.sh | sh
```

The script downloads the prebuilt binary for your platform from the latest
GitHub release and verifies its SHA-256 checksum before installing it.

## Cargo

Available once the `kitchn` crate is published with the first release.

```sh
cargo install kitchn --locked
```

## From source

This works today. kitchn needs the Rust toolchain pinned in the repository's
`rust-toolchain.toml`; rustup installs it automatically.

```sh
git clone https://github.com/lemarier/kitchen
cd kitchen
cargo install --path crates/kitchen-cli --locked
```

:::note
The rename from `kitchen` to `kitchn` is in progress
([#18](https://github.com/lemarier/kitchen/issues/18)). Until it lands, a
source build installs the binary as `kitchen`, and the install script and
crate are not published yet.
:::

## Check the install

```sh
kitchn --version
kitchn --help
```

Next: [set up your first house](/docs/start/quickstart/). It takes about two
minutes.
