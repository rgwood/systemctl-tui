default:
  @just --list

fmt:
    cargo fmt
    prettier --write .
    just --fmt --unstable

update:
    cargo upgrade --incompatible
    cargo update

check:
    pre-commit run --all-files
    cargo check
    cargo clippy

build:
    cargo build --all-targets

test:
    cargo test run --workspace --all-targets

changelog:
    git cliff -o CHANGELOG.md
    prettier --write CHANGELOG.md
