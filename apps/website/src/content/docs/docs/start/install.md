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
`rust-toolchain.toml`; rustup installs it automatically. The install step also needs [just](https://just.systems).

```sh
git clone https://github.com/lemarier/kitchen
cd kitchen
just install
```

`just install` runs `cargo install` and records the commit you built, which guided `house init` needs to pin its built-in guidance. It records it only from a clean checkout whose commit is on a remote branch; otherwise it prints why and installs without it. That binary still works, but `house init` then needs `--bundle <path>`.

:::note
The install script and the `kitchn` crate are not published yet
([#18](https://github.com/lemarier/kitchen/issues/18)). A source build
installs the `kitchn` binary today.
:::

## Check the install

```sh
kitchn --version
kitchn --help
```

Next: [set up your first house](/docs/start/quickstart/). It takes about two
minutes.
