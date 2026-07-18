set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := true

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
    @echo "Build size: $(du -h target/release/{{expected_filename}} | cut -f1)"

publish-to-local-bin: build-release
    cp target/release/{{expected_filename}} ~/bin/

build-linux-x64:
    cross build --target x86_64-unknown-linux-musl --release
    
build-linux-arm64:
    cross build --target aarch64-unknown-linux-musl --release

build-windows-on-linux:
    cross build --target x86_64-pc-windows-gnu --release

# Host lists and destinations come from the ignored `.env` file. See `.env.example`.
publish-everywhere: build-release build-linux-x64 build-linux-arm64
    @local_destination="${PUBLISH_LOCAL_DESTINATION/#\~/$HOME}"; mkdir -p "$local_destination"; echo "Installing locally: $local_destination/{{expected_filename}}"; cp target/release/{{expected_filename}} "$local_destination"
    @while IFS= read -r host; do [ -z "$host" ] || { echo "Publishing x86_64 binary to $host"; rsync --archive --human-readable --info=progress2,name0 target/x86_64-unknown-linux-musl/release/{{expected_filename}} "$host:$PUBLISH_REMOTE_DESTINATION"; }; done < <(tr ',' '\n' <<< "$PUBLISH_X86_64_HOSTS")
    @while IFS= read -r host; do [ -z "$host" ] || { echo "Publishing ARM64 binary to $host"; rsync --archive --human-readable --info=progress2,name0 target/aarch64-unknown-linux-musl/release/{{expected_filename}} "$host:$PUBLISH_REMOTE_DESTINATION"; }; done < <(tr ',' '\n' <<< "$PUBLISH_AARCH64_HOSTS")
