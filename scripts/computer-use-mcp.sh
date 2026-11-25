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
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
if [ -n "${COMPUTER_USE_LINUX_BINARY:-}" ]; then
	server_binary=$COMPUTER_USE_LINUX_BINARY
else
	# Prefer the static release build, then a native release build.
	for candidate in \
		"$repo_root/target/$(uname -m)-unknown-linux-musl/release/computer-use-linux" \
		"$repo_root/target/release/computer-use-linux"; do
		if [ -x "$candidate" ]; then
			server_binary=$candidate
			break
		fi
	done
	server_binary=${server_binary:-"$repo_root/target/$(uname -m)-unknown-linux-musl/release/computer-use-linux"}
fi
if [ ! -x "$server_binary" ]; then
	printf '%s\n' "computer-use-linux binary not found: $server_binary (run cargo build --release, or set COMPUTER_USE_LINUX_BINARY)" >&2
	exit 1
fi
exec "$server_binary" --stdio
