set shell := ["nu", "-c"]

default:
  @just --list

watch:
    watchexec --exts=rs --on-busy-update=restart -- cargo run

run:
    cargo run

test:
    cargo test

watch-tests:
    watchexec --exts=rs -- cargo test

expected_filename := "systemctl-tui"

build-release:
    cargo build --release
    @$"Build size: (ls target/release/{{expected_filename}} | get size)"

publish-to-local-bin: build-release
    cp target/release/{{expected_filename}} ~/bin/

build-linux-x64:
    cross build --target x86_64-unknown-linux-musl --release

build-linux-arm64:
    cross build --target aarch64-unknown-linux-musl --release

build-windows-on-linux:
    cross build --target x86_64-pc-windows-gnu --release



publish-potato-pi: build-linux-arm64
    rsync target/aarch64-unknown-linux-musl/release/systemctl-tui potato-pi:~/bin/