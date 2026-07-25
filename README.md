# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A fast, simple TUI for interacting with [systemd](https://en.wikipedia.org/wiki/Systemd) units and their logs.
<img width="1800" height="1222" alt="image" src="https://github.com/user-attachments/assets/924d3cf2-cac6-474b-9127-756520fede6d" />

`systemctl-tui` can quickly browse service and timer status and logs, manage services and timers, and view/edit unit files. It aims to do a small number of things well.

## Install

Note: this project only works on Linux (WSL works _if_ you [have systemd enabled](https://devblogs.microsoft.com/commandline/systemd-support-is-now-available-in-wsl/)). Binaries are published for x64 and ARM64 in the [GitHub releases](https://github.com/rgwood/systemctl-tui/releases), and [distro packages](#distro-packages) are available.

### Binary Release

Automated install/update (don't forget to always verify what you're piping into bash):

```sh
curl https://raw.githubusercontent.com/rgwood/systemctl-tui/master/install.sh | bash
```
The script installs the downloaded binary to `$HOME/.local/bin` by default, but it can be changed by setting the `DIR` environment variable.

### Debian/Ubuntu

`.deb` packages (with no dependencies) are published in the [GitHub releases](https://github.com/rgwood/systemctl-tui/releases). Download the one for your architecture and install it with:

```sh
sudo apt install ./systemctl-tui_*.deb
```

### Rust

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

## Remote hosts

`systemctl-tui --host user@hostname` and `systemctl-tui --remote` manage a remote machine over SSH, no remote install needed.

The remote host needs `systemd-stdio-bridge` (part of the core systemd package on all major distros; verified working on systemd 249 and 255, and the flags we use exist back to at least systemd 239) and `journalctl`. Viewing user-scope services requires a running user manager on the remote host (an active session, or lingering enabled via `loginctl enable-linger`). Editing unit files remotely is not supported yet.

## Credits

- Inspired by the truly wonderful [Lazygit](https://github.com/jesseduffield/lazygit)
- [`sysz`](https://github.com/joehillen/sysz) is so cool
- Used [`ratatui-template`](https://github.com/kdheepak/ratatui-template/) to get started
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
