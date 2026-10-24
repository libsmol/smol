#!/usr/bin/env python3

import json
import sys
import time

TOOLS = [
    {
        "name": "add",
        "description": "Add two numbers",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer", "description": "First number"},
                "b": {"type": "integer"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "fail",
        "description": "Always fails",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "wait",
        "description": "Sleeps, then answers",
        "inputSchema": {
            "type": "object",
            "properties": {"seconds": {"type": "number"}},
        },
    },
    {
        "name": "echo-text",
        "description": "Echo the text back",
        "inputSchema": {
            "type": "object",
            "properties": {"the-text": {"type": "string"}},
        },
    },
    {
        "name": "mixed",
        "description": "Answers with text, an embedded file, an image and a link",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "exit",
        "description": "Exits without answering",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "cancelled",
        "description": "Counts the cancellations received",
        "inputSchema": {"type": "object", "properties": {}},
    },
]

CANCELLED = []

MIXED = [
    {"type": "text", "text": "Intro"},
    {
        "type": "resource",
        "resource": {"uri": "file:///notes.txt", "mimeType": "text/plain", "text": "embedded content"},
    },
    {"type": "image", "data": "iVBORw0KGgpoZWxsbw==", "mimeType": "image/png"},
    {"type": "resource_link", "uri": "file:///big.bin", "name": "big"},
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def result(msg_id, payload):
    send({"jsonrpc": "2.0", "id": msg_id, "result": payload})


def handle(msg):
    method = msg.get("method")
    msg_id = msg.get("id")
    params = msg.get("params", {}) or {}

    if method == "initialize":
        result(
            msg_id,
            {
                "protocolVersion": params["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-mcp-server", "version": "0.1.0"},
            },
        )
    elif method == "notifications/initialized":
        pass
    elif method == "notifications/cancelled":
        CANCELLED.append(params.get("requestId"))
    elif method == "tools/list":
        # Two pages, the last one with an empty cursor
        if params.get("cursor"):
            result(msg_id, {"tools": TOOLS[3:], "nextCursor": ""})
        else:
            result(msg_id, {"tools": TOOLS[:3], "nextCursor": "2"})
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments", {}) or {}
        if name == "add":
            result(
                msg_id,
                {"content": [{"type": "text", "text": str(int(args["a"]) + int(args["b"]))}]},
            )
        elif name == "fail":
            result(
                msg_id,
                {"content": [{"type": "text", "text": "boom"}], "isError": True},
            )
        elif name == "wait":
            time.sleep(float(args["seconds"]))
            result(msg_id, {"content": [{"type": "text", "text": "awake"}]})
        elif name == "echo-text":
            result(msg_id, {"content": [{"type": "text", "text": args["the-text"]}]})
        elif name == "mixed":
            result(msg_id, {"content": MIXED})
        elif name == "exit":
            sys.exit(0)
        elif name == "cancelled":
            result(msg_id, {"content": [{"type": "text", "text": str(len(CANCELLED))}]})
        else:
            send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "unknown tool"}})


def main():
    # MCP messages are UTF-8 but Windows reads stdin in the system code page by default
    sys.stdin.reconfigure(encoding="utf-8")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
