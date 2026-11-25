#!/bin/sh
set -eu

# MCP clients such as Claude Code or Codex may start this script without the
# graphical Login Session environment. Fill in only the variables that are
# missing, so a caller that already runs inside the desktop keeps its own.
while IFS= read -r variable; do
	case "$variable" in
		WAYLAND_DISPLAY=*|HYPRLAND_INSTANCE_SIGNATURE=*|XDG_RUNTIME_DIR=*|DBUS_SESSION_BUS_ADDRESS=*|XDG_CURRENT_DESKTOP=*|XDG_SESSION_DESKTOP=*|XDG_SESSION_TYPE=*|DISPLAY=*)
			name=${variable%%=*}
			eval "current=\${$name:-}"
			if [ -z "$current" ]; then
				export "$variable"
			fi
			;;
	esac
done <<EOF
$(systemctl --user show-environment 2>/dev/null || true)
EOF

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# Inside a source checkout this script lives in plugin/scripts/; an installed
# Claude Code plugin is copied on its own, so the checkout paths simply do not exist.
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
arch=$(uname -m)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}

find_server() {
	if [ -n "${COMPUTER_USE_LINUX_BINARY:-}" ]; then
		printf '%s\n' "$COMPUTER_USE_LINUX_BINARY"
		return
	fi
	# In a checkout, use the more recently built of the static and native builds.
	static_build="$repo_root/target/$arch-unknown-linux-musl/release/computer-use-linux"
	native_build="$repo_root/target/release/computer-use-linux"
	if [ -x "$static_build" ] && [ -x "$native_build" ] && [ "$native_build" -nt "$static_build" ]; then
		printf '%s\n' "$native_build"
		return
	fi
	for candidate in \
		"$static_build" \
		"$native_build" \
		"$data_home/computer-use-linux/bin/computer-use-linux" \
		"$HOME/.local/bin/computer-use-linux" \
		"$HOME/.cargo/bin/computer-use-linux"; do
		if [ -x "$candidate" ]; then
			printf '%s\n' "$candidate"
			return
		fi
	done
	command -v computer-use-linux 2>/dev/null || true
}

server_binary=$(find_server)
if [ -z "$server_binary" ] || [ ! -x "$server_binary" ]; then
	cat >&2 <<MESSAGE
computer-use-linux server binary not found${server_binary:+: $server_binary}.
Install it with one of:
  - run scripts/install-release.sh from https://github.com/gzigurella/computer-use
    (installs the latest release into $data_home/computer-use-linux/bin);
  - cargo install --locked --git https://github.com/gzigurella/computer-use computer-use-linux;
  - build a checkout with cargo build --release;
or set COMPUTER_USE_LINUX_BINARY to the binary's path.
MESSAGE
	exit 1
fi
exec "$server_binary" --stdio
