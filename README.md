# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A simple little TUI for interacting with systemd services and their logs.

![image](https://github.com/rgwood/systemctl-tui/assets/26268125/772eb23d-1e7e-4a31-a38c-01c0ac435bc2)


## Install

Currently you need [Rust installed](https://rustup.rs/). Either:

1. Run `cargo install systemctl-tui`
2. Clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`

Optional: alias `systemctl-tui` to `st` for quick access

## Future Work

This was thrown together in a weekend. It currently only supports read operations (so no stopping/starting services). More features to come.

## Credits

- Inspired by the truly excellent [Lazygit](https://github.com/jesseduffield/lazygit).
- Based on the excellent [`ratatui-template`](https://github.com/kdheepak/ratatui-template/)
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
