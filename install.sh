#!/bin/bash

# allow specifying different destination directory
DIR="${DIR:-"$HOME/.local/bin"}"

# map different architecture variations to the available binaries
ARCH=$(uname -m)
case $ARCH in
    i386|i686) ARCH=x86 ;;
    aarch64*) ARCH=arm64 ;;
esac

# prepare the download URL
GITHUB_LATEST_VERSION=$(curl -L -s -H 'Accept: application/json' https://github.com/rgwood/systemctl-tui/releases/latest | sed -e 's/.*"tag_name":"\([^"]*\)".*/\1/')
GITHUB_FILE="systemctl-tui-${ARCH}-unknown-linux-musl.tar.gz"
GITHUB_URL="https://github.com/rgwood/systemctl-tui/releases/download/${GITHUB_LATEST_VERSION}/${GITHUB_FILE}"

# install/update the local binary
curl -L -o systemctl-tui.tar.gz $GITHUB_URL
tar xzvf systemctl-tui.tar.gz systemctl-tui
install -Dm 755 systemctl-tui -t "$DIR"
rm systemctl-tui systemctl-tui.tar.gz
