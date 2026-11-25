"""Strict MCP result validation shared by the conformance test and smoke clients.

Real MCP clients such as Claude Code reject replies that break the MCP schema,
so the test clients must not be more lenient than they are.
"""

import base64
import binascii
import json
import re

TOOL_NAME = re.compile(r"^[A-Za-z0-9_-]{1,64}$")


class McpViolation(AssertionError):
    pass


def _require(condition, message):
    if not condition:
        raise McpViolation(message)


def validate_content_block(block, where="content block"):
    _require(isinstance(block, dict), f"{where} is not an object: {block!r}")
    kind = block.get("type")
    if kind == "text":
        _require(isinstance(block.get("text"), str), f"{where} text block has no string text")
    elif kind in ("image", "audio"):
        _require(isinstance(block.get("mimeType"), str), f"{where} {kind} block has no mimeType")
        data = block.get("data")
        _require(isinstance(data, str) and data, f"{where} {kind} block has no base64 data")
        try:
            base64.b64decode(data, validate=True)
        except (binascii.Error, ValueError) as error:
            raise McpViolation(f"{where} {kind} data is not valid base64: {error}") from error
    elif kind == "resource_link":
        _require(isinstance(block.get("uri"), str), f"{where} resource_link has no uri")
        _require(isinstance(block.get("name"), str), f"{where} resource_link has no name")
    elif kind == "resource":
        resource = block.get("resource")
        _require(isinstance(resource, dict) and isinstance(resource.get("uri"), str),
                 f"{where} embedded resource has no uri")
    else:
        raise McpViolation(f"{where} has unknown type {kind!r}")


def validate_tool_result(result):
    """Raise McpViolation unless result is a valid CallToolResult."""
    _require(isinstance(result, dict), f"tool result is not an object: {result!r}")
    content = result.get("content")
    _require(isinstance(content, list),
             f"tool result has no content array (keys: {sorted(result)})")
    for index, block in enumerate(content):
        validate_content_block(block, f"content[{index}]")
    if "isError" in result:
        _require(isinstance(result["isError"], bool), "isError is not a boolean")
    if "structuredContent" in result:
        structured = result["structuredContent"]
        _require(isinstance(structured, dict), "structuredContent is not an object")
        # The spec asks servers to mirror structured output as JSON text for older clients.
        mirrored = [
            block for block in content
            if block.get("type") == "text" and _parses_to(block["text"], structured)
        ]
        _require(mirrored, "structuredContent is not mirrored by a JSON text content block")
    return result


def _parses_to(text, expected):
    try:
        return json.loads(text) == expected
    except (TypeError, json.JSONDecodeError):
        return False


def structured(result):
    """Validate a tool result and return its structured payload."""
    validate_tool_result(result)
    value = result.get("structuredContent")
    if isinstance(value, dict):
        return value
    for block in result["content"]:
        if block.get("type") == "text":
            try:
                value = json.loads(block["text"])
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                return value
    raise McpViolation("tool result has no structured JSON payload")


def validate_tool_definition(tool):
    _require(isinstance(tool, dict), f"tool definition is not an object: {tool!r}")
    name = tool.get("name")
    _require(isinstance(name, str) and TOOL_NAME.match(name),
             f"tool name {name!r} must match {TOOL_NAME.pattern}")
    _require(isinstance(tool.get("description", ""), str), f"{name}: description is not a string")
    schema = tool.get("inputSchema")
    _require(isinstance(schema, dict) and schema.get("type") == "object",
             f"{name}: inputSchema must be a JSON Schema object with type=object")
    _require(isinstance(schema.get("properties", {}), dict), f"{name}: properties is not an object")
    for field in schema.get("required", []):
        _require(field in schema.get("properties", {}),
                 f"{name}: required field {field!r} is not declared in properties")
    if "outputSchema" in tool:
        _require(isinstance(tool["outputSchema"], dict) and tool["outputSchema"].get("type") == "object",
                 f"{name}: outputSchema must have type=object")
    if "annotations" in tool:
        annotations = tool["annotations"]
        _require(isinstance(annotations, dict), f"{name}: annotations is not an object")
        for hint in ("readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"):
            if hint in annotations:
                _require(isinstance(annotations[hint], bool), f"{name}: {hint} is not a boolean")


_JSON_TYPES = {
    "object": dict,
    "array": list,
    "string": str,
    "boolean": bool,
    "null": type(None),
}


def _is_type(value, name):
    if name == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if name == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    return isinstance(value, _JSON_TYPES[name])


def validate_against_schema(value, schema, path="structuredContent"):
    """Check the JSON Schema subset the server's output schemas use:
    type, required, properties, items and enum."""
    types = schema.get("type")
    if types is not None:
        names = types if isinstance(types, list) else [types]
        _require(any(_is_type(value, name) for name in names),
                 f"{path} should be {types}, got {type(value).__name__}: {value!r:.120}")
    if "enum" in schema:
        _require(value in schema["enum"], f"{path} should be one of {schema['enum']}, got {value!r}")
    if isinstance(value, dict):
        for field in schema.get("required", []):
            _require(field in value, f"{path} is missing required field {field!r}")
        for field, subschema in schema.get("properties", {}).items():
            if field in value:
                validate_against_schema(value[field], subschema, f"{path}.{field}")
    if isinstance(value, list) and "items" in schema:
        for index, item in enumerate(value):
            validate_against_schema(item, schema["items"], f"{path}[{index}]")


def validate_tool_output(result, tool):
    """Validate a successful result's structuredContent against the tool's outputSchema."""
    validate_tool_result(result)
    schema = tool.get("outputSchema")
    if schema is None or result.get("isError"):
        return
    _require("structuredContent" in result,
             f"{tool['name']} declares outputSchema but returned no structuredContent")
    validate_against_schema(result["structuredContent"], schema, f"{tool['name']}.structuredContent")
