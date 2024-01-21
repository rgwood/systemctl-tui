# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A fast, simple TUI for interacting with [systemd](https://en.wikipedia.org/wiki/Systemd) services and their logs.

![image](https://github.com/rgwood/systemctl-tui/assets/26268125/ac427818-ab9e-4b04-bce4-e41cfb8ecb25)

`systemctl-tui` can quickly browse service status and logs, and start/stop/restart services. It aims to do a small number of things well.

## Install

Note: this project only works on Linux. Binaries are published for x64 and ARM64 in the GitHub releases, and [a Nix package](https://search.nixos.org/packages?query=systemctl-tui) is available.

If you'd rather build from scratch you will need [Rust installed](https://rustup.rs/). Then either:

1. Run `cargo install systemctl-tui`
2. Clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`

#### Optional:

1. Alias `systemctl-tui` to `st` for quick access
2. Create a symlink so `systemctl-tui` can be used with sudo:
```sh
sudo ln -s ~/.cargo/bin/systemctl-tui /usr/bin/systemctl-tui
```

## Help
![image](https://github.com/rgwood/systemctl-tui/assets/26268125/83e26502-665b-41a7-9940-b0c03d054e9a)

## Credits

- Inspired by the truly wonderful [Lazygit](https://github.com/jesseduffield/lazygit)
- [`sysz`](https://github.com/joehillen/sysz) is so cool
- Used [`ratatui-template`](https://github.com/kdheepak/ratatui-template/) to get started
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
