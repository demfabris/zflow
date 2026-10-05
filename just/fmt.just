import 'common.just'

set working-directory := ".."

[default]
[private]
default:
    @just --list fmt

# Format Rust code.
apply:
    cargo fmt --all

# Check formatting without changing files.
check:
    cargo fmt --all --check
