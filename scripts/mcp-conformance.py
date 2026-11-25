#!/usr/bin/env python3
"""Check the stdio MCP server against the MCP schema rules real clients enforce.

Needs no desktop and no third-party packages: tool calls that would touch the
desktop are made with arguments that must fail cleanly, and computer_status
only reports what the environment exposes.

Usage: python3 scripts/mcp-conformance.py PATH/TO/computer-use-linux
"""

import json
import subprocess
import sys
from pathlib import Path

from mcp_result import McpViolation, validate_tool_definition, validate_tool_result

LEGACY_VERSIONS = ["2025-03-26", "2025-06-18", "2025-11-25"]
NEWEST_LEGACY_VERSION = "2025-11-25"
MODERN_VERSION = "2026-07-28"
EXPECTED_TOOLS = {"computer_status", "computer_start", "computer_observe", "computer_act", "computer_stop"}
CLIENT_INFO = {"name": "mcp-conformance", "version": "1"}


class Server:
    def __init__(self, binary):
        self.process = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.next_id = 0

    def send_raw(self, line):
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()

    def read(self):
        line = self.process.stdout.readline()
        if not line:
            raise McpViolation("server closed stdout")
        message = json.loads(line)
        check(message.get("jsonrpc") == "2.0", f"response is not JSON-RPC 2.0: {message}")
        check(("result" in message) != ("error" in message),
              f"response must have exactly one of result or error: {message}")
        if "error" in message:
            error = message["error"]
            check(isinstance(error.get("code"), int) and isinstance(error.get("message"), str),
                  f"malformed JSON-RPC error: {error}")
        return message

    def request(self, method, params=None):
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            message["params"] = params
        self.send_raw(json.dumps(message))
        response = self.read()
        check(response.get("id") == self.next_id,
              f"{method}: response id {response.get('id')!r} != request id {self.next_id}")
        return response

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.send_raw(json.dumps(message))

    def initialize(self, version):
        response = self.request("initialize", {
            "protocolVersion": version, "capabilities": {}, "clientInfo": CLIENT_INFO,
        })
        check("result" in response, f"initialize {version} failed: {response}")
        self.notify("notifications/initialized")
        return response["result"]

    def call_tool(self, name, arguments):
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        check("result" in response, f"tools/call {name} returned a JSON-RPC error: {response}")
        return validate_tool_result(response["result"])

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.kill()
            raise McpViolation("server did not exit after stdin EOF")
        check(self.process.returncode == 0, f"server exited with status {self.process.returncode}")


def check(condition, message):
    if not condition:
        raise McpViolation(message)


def check_initialize(binary):
    for requested in LEGACY_VERSIONS + ["2099-01-01"]:
        server = Server(binary)
        try:
            result = server.initialize(requested)
            expected = requested if requested in LEGACY_VERSIONS else NEWEST_LEGACY_VERSION
            check(result.get("protocolVersion") == expected,
                  f"initialize {requested}: negotiated {result.get('protocolVersion')!r}, expected {expected!r}")
            check(isinstance(result.get("capabilities", {}).get("tools"), dict),
                  f"initialize {requested}: capabilities.tools missing")
            info = result.get("serverInfo", {})
            check(isinstance(info.get("name"), str) and isinstance(info.get("version"), str),
                  f"initialize {requested}: serverInfo needs name and version")
            check(isinstance(result.get("instructions"), str) and result["instructions"],
                  f"initialize {requested}: instructions missing")
            # A notification must not produce a response: the next line answers the ping.
            check("result" in server.request("ping"), "ping after notifications/initialized failed")
        finally:
            server.close()


def check_session(binary):
    server = Server(binary)
    try:
        server.initialize(NEWEST_LEGACY_VERSION)

        tools = server.request("tools/list")
        check("result" in tools, f"tools/list failed: {tools}")
        definitions = tools["result"].get("tools")
        check(isinstance(definitions, list), "tools/list result has no tools array")
        for tool in definitions:
            validate_tool_definition(tool)
        names = {tool["name"] for tool in definitions}
        check(names == EXPECTED_TOOLS, f"unexpected tool set: {sorted(names)}")

        status = server.call_tool("computer_status", {})
        check(status.get("isError") is False, f"computer_status reported an error: {status}")
        payload = status.get("structuredContent", {})
        for backend in payload.get("available_backends", []):
            check(isinstance(backend.get("target"), str), f"backend without target: {backend}")

        # Every failure must still be a well-formed tool result with isError=true.
        for name, arguments in [
            ("computer_observe", {"session_id": "no-such-session"}),
            ("computer_act", {"session_id": "no-such-session", "action": {"type": "wait"}}),
            ("computer_stop", {"session_id": "no-such-session"}),
            ("computer_start", {"output": "a", "overlay_monitor": "b"}),
            ("no_such_tool", {}),
        ]:
            result = server.call_tool(name, arguments)
            check(result.get("isError") is True, f"{name} with invalid arguments did not fail: {result}")

        unknown = server.request("no/such/method")
        check(unknown.get("error", {}).get("code") == -32601,
              f"unknown method should return -32601: {unknown}")

        server.send_raw("{not json")
        parse_error = server.read()
        check(parse_error.get("id") is None and parse_error.get("error", {}).get("code") == -32700,
              f"malformed JSON should return -32700 with id null: {parse_error}")
        check("result" in server.request("ping"), "server stopped answering after a parse error")
    finally:
        server.close()


def check_modern_discovery(binary):
    server = Server(binary)
    try:
        meta = {
            "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": CLIENT_INFO,
        }
        discovery = server.request("server/discover", {"_meta": meta})
        check("result" in discovery, f"server/discover failed: {discovery}")
        supported = discovery["result"].get("supportedVersions", [])
        check(MODERN_VERSION in supported and all(v in supported for v in LEGACY_VERSIONS),
              f"supportedVersions incomplete: {supported}")
        check(isinstance(discovery["result"].get("instructions"), str), "server/discover has no instructions")
        status = server.request("tools/call", {"name": "computer_status", "arguments": {}, "_meta": meta})
        check("result" in status, f"modern computer_status failed: {status}")
        validate_tool_result(status["result"])
    finally:
        server.close()


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[-1])
    binary = str(Path(sys.argv[1]).resolve())
    for check_name, run in [
        ("initialize negotiation", check_initialize),
        ("tools/list and tools/call results", check_session),
        ("modern server/discover", check_modern_discovery),
    ]:
        try:
            run(binary)
        except McpViolation as error:
            sys.exit(f"MCP conformance failed ({check_name}): {error}")
        print(f"ok: {check_name}")
    print("MCP conformance passed")


if __name__ == "__main__":
    main()
