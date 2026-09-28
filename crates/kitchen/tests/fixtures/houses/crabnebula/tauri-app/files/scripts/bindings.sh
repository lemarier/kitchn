#!/bin/sh
set -eu
cargo test --workspace --locked export_bindings
