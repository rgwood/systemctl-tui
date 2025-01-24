# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A fast, simple TUI for interacting with [systemd](https://en.wikipedia.org/wiki/Systemd) services and their logs.
![Screenshot from 2025-01-23 21-44-31](https://github.com/user-attachments/assets/caac6034-d4e3-4c54-8163-24a8a6d39cb4)

`systemctl-tui` can quickly browse service status and logs, start/stop/restart/reload services, and view/edit unit files. It aims to do a small number of things well.

## Install

Note: this project only works on Linux (WSL works _if_ you [have systemd enabled](https://devblogs.microsoft.com/commandline/systemd-support-is-now-available-in-wsl/)). Binaries are published for x64 and ARM64 in the GitHub releases, and [distro packages](#distro-packages) are available.

If you'd rather build from scratch you will need [Rust installed](https://rustup.rs/). Then either:

1. Run `cargo install systemctl-tui --locked`
2. Clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`

### Distro Packages

<details>
  <summary>Packaging status</summary>

[![Packaging status](https://repology.org/badge/vertical-allrepos/systemctl-tui.svg)](https://repology.org/project/systemctl-tui/versions)

</details>

#### Arch Linux

`systemctl-tui` can be installed from the [official repositories](https://archlinux.org/packages/extra/x86_64/systemctl-tui/):

```sh
pacman -S systemctl-tui
```

#### Nix

[A Nix package](https://search.nixos.org/packages?query=systemctl-tui) is available and can be installed as follows:

```sh
nix-shell -p systemctl-tui
```

#### Optional:

1. Alias `systemctl-tui` to `st` for quick access
2. Create a symlink so `systemctl-tui` can be used with sudo:

```sh
sudo ln -s ~/.cargo/bin/systemctl-tui /usr/bin/systemctl-tui
```

## Help
![image](https://github.com/rgwood/systemctl-tui/assets/26268125/b1b49850-61c4-4667-9110-20a34f917055)

## Credits

- Inspired by the truly wonderful [Lazygit](https://github.com/jesseduffield/lazygit)
- [`sysz`](https://github.com/joehillen/sysz) is so cool
- Used [`ratatui-template`](https://github.com/kdheepak/ratatui-template/) to get started
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
