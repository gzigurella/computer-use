#!/bin/sh
set -eu

while IFS= read -r variable; do
	case "$variable" in
		WAYLAND_DISPLAY=*|HYPRLAND_INSTANCE_SIGNATURE=*|XDG_RUNTIME_DIR=*|DBUS_SESSION_BUS_ADDRESS=*|XDG_CURRENT_DESKTOP=*|XDG_SESSION_TYPE=*|DISPLAY=*)
			export "$variable"
			;;
	esac
done <<EOF
$(systemctl --user show-environment)
EOF

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
server_binary=${COMPUTER_USE_LINUX_BINARY:-"$repo_root/target/x86_64-unknown-linux-musl/release/computer-use-linux"}
if [ ! -x "$server_binary" ]; then
	printf '%s\n' "computer-use-linux binary not found: $server_binary" >&2
	exit 1
fi
exec "$server_binary" --stdio
