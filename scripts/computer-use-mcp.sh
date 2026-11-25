#!/bin/sh
# Compatibility entry point for MCP clients configured with this path (for
# example Codex or the project .mcp.json). The launcher lives with the Claude
# Code plugin so that an installed plugin carries its own copy.
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$script_dir/../plugin/scripts/computer-use-mcp.sh" "$@"
