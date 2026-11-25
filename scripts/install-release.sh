#!/bin/sh
# Install a computer-use-linux release binary where the MCP launcher finds it:
#   $XDG_DATA_HOME/computer-use-linux/bin/computer-use-linux
#
# Usage: scripts/install-release.sh [TAG]     (default: the latest release)
# Needs curl, tar and sha256sum. The download is checked against the
# release's published SHA-256 before anything is installed.
set -eu

repository=${COMPUTER_USE_LINUX_REPOSITORY:-gzigurella/computer-use}
arch=$(uname -m)
if [ "$arch" != "x86_64" ]; then
	printf '%s\n' "Releases are built for x86_64 only; build from source on $arch (see README)." >&2
	exit 1
fi

tag=${1:-}
if [ -z "$tag" ]; then
	tag=$(curl -fsSL "https://api.github.com/repos/$repository/releases/latest" |
		sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
	if [ -z "$tag" ]; then
		printf '%s\n' "could not determine the latest release of $repository" >&2
		exit 1
	fi
fi

name="computer-use-linux-$tag-$arch-unknown-linux-musl"
base=${COMPUTER_USE_LINUX_RELEASE_BASE:-"https://github.com/$repository/releases/download/$tag"}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

curl -fsSL -o "$work/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$work/$name.tar.gz.sha256" "$base/$name.tar.gz.sha256"
(cd "$work" && sha256sum -c "$name.tar.gz.sha256")
tar -C "$work" -xzf "$work/$name.tar.gz"

destination=${XDG_DATA_HOME:-"$HOME/.local/share"}/computer-use-linux
mkdir -p "$destination/bin"
install -m 0755 "$work/$name/computer-use-linux" "$destination/bin/computer-use-linux.new"
mv "$destination/bin/computer-use-linux.new" "$destination/bin/computer-use-linux"
rm -rf "$destination/licenses"
cp -r "$work/$name/LICENSE" "$work/$name/licenses" "$work/$name/BUNDLED-XKB-DATA" "$destination/"
printf '%s\n' "Installed computer-use-linux $tag to $destination/bin/computer-use-linux"
