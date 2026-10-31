#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 || ( $# -eq 2 && "$2" != "--qt" ) ]]; then
  echo "usage: $0 path/to/computer-use-linux [--qt]" >&2
  exit 2
fi

server=$(realpath "$1")
parent_wayland_socket=""
if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
  if [[ "$WAYLAND_DISPLAY" == /* ]]; then
    parent_wayland_socket="$WAYLAND_DISPLAY"
  else
    parent_wayland_socket="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
  fi
  if [[ ! -S "$parent_wayland_socket" ]]; then
    echo "the active Wayland socket is unavailable: $parent_wayland_socket" >&2
    exit 1
  fi
fi
temporary=$(mktemp -d /tmp/cu.XXXXXX)
runtime="$temporary/r"
mkdir -m 700 "$runtime"
config="$temporary/hyprland.lua"
: > "$config"

export XDG_RUNTIME_DIR="$runtime"
export XDG_SESSION_TYPE=wayland
export XDG_CURRENT_DESKTOP=Hyprland
export XDG_SESSION_DESKTOP=Hyprland
export HYPRLAND_NO_SD_VARS=1
export COMPUTER_USE_REQUIRE_ATSPI_ACTIONS="${COMPUTER_USE_REQUIRE_ATSPI_ACTIONS:-1}"
export COMPUTER_USE_REQUIRE_CLIPBOARD="${COMPUTER_USE_REQUIRE_CLIPBOARD:-1}"
export COMPUTER_USE_XKB_LAYOUT_MATRIX=1
for render_node in /dev/dri/renderD*; do
  if [[ -e "$render_node" ]]; then
    export AQ_NO_KMS_REQUIREMENT=1
    export AQ_DRM_DEVICES="$render_node"
    break
  fi
done
unset HYPRLAND_INSTANCE_SIGNATURE
if [[ -n "$parent_wayland_socket" ]]; then
  export WAYLAND_DISPLAY="$parent_wayland_socket"
else
  unset WAYLAND_DISPLAY
fi

Hyprland --config "$config" >"$temporary/hyprland.log" 2>&1 &
hyprland_pid=$!

cleanup() {
  result=$?
  trap - EXIT
  kill -TERM "$hyprland_pid" 2>/dev/null || true
  wait "$hyprland_pid" 2>/dev/null || true
  if (( result != 0 )); then
    cat "$temporary/hyprland.log" >&2
    find "$runtime/hypr" -type f -name hyprland.log -exec cat {} \; 2>/dev/null || true
  fi
  unlink "$temporary/hyprland.log" 2>/dev/null || true
  unlink "$config" 2>/dev/null || true
  find "$runtime" -depth \( -type f -o -type s \) -delete 2>/dev/null || true
  find "$runtime" -depth -type d -empty -delete 2>/dev/null || true
  rmdir "$temporary" 2>/dev/null || true
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

deadline=$((SECONDS + 30))
while (( SECONDS < deadline )); do
  if ! kill -0 "$hyprland_pid" 2>/dev/null; then
    wait "$hyprland_pid" || true
    echo "Hyprland exited before becoming ready" >&2
    exit 1
  fi
  instance_info=$(hyprctl -j instances 2>/dev/null || true)
  if [[ -n "$instance_info" ]]; then
    mapfile -t instance < <(python3 -c 'import json,sys; pid=int(sys.argv[1]); matches=[entry for entry in json.load(sys.stdin) if entry.get("pid")==pid]; sys.exit(1) if len(matches)!=1 else None; print(matches[0]["instance"]); print(matches[0]["wl_socket"])' "$hyprland_pid" <<<"$instance_info" 2>/dev/null || true)
    if [[ ${#instance[@]} == 2 ]]; then
      export HYPRLAND_INSTANCE_SIGNATURE="${instance[0]}"
      export WAYLAND_DISPLAY="${instance[1]}"
      break
    fi
  fi
  sleep 0.2
done

if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" || -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "Hyprland did not expose an IPC and Wayland socket within 30 seconds" >&2
  exit 1
fi

deadline=$((SECONDS + 15))
while (( SECONDS < deadline )); do
  monitors=$(hyprctl -j monitors 2>/dev/null || true)
  if [[ -n "$monitors" ]] && python3 -c 'import json,sys; sys.exit(not json.load(sys.stdin))' <<<"$monitors"; then
    break
  fi
  sleep 0.2
done

if [[ -z "${monitors:-}" ]] || ! python3 -c 'import json,sys; sys.exit(not json.load(sys.stdin))' <<<"$monitors"; then
  echo "Hyprland did not expose its headless output" >&2
  exit 1
fi

python3 scripts/hyprland-http-smoke.py "$server"

smoke_args=(--headless-output --hyprland-only-build)
if [[ "${2:-}" == "--qt" ]]; then
  smoke_args+=(--qt)
fi
python3 scripts/hyprland-smoke.py "${smoke_args[@]}" "$server"

clipboard_sentinel='preexisting computer-use clipboard text'
printf '%s' "$clipboard_sentinel" | wl-copy --type text/plain;charset=utf-8
python3 scripts/hyprland-smoke.py "${smoke_args[@]}" "$server"
restored_clipboard=$(wl-paste --no-newline --type text/plain;charset=utf-8)
if [[ "$restored_clipboard" != "$clipboard_sentinel" ]]; then
  echo "Hyprland smoke did not preserve existing clipboard text" >&2
  exit 1
fi
python3 scripts/hyprland-smoke.py "${smoke_args[@]}" --xkb-fallback "$server"
if [[ "${2:-}" != "--qt" ]]; then
  python3 scripts/hyprland-smoke.py --high-resolution-output --hyprland-only-build "$server"
fi
