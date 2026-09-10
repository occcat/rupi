#!/usr/bin/env python3
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "echo", "version": "1.0"},
                },
            }
        )
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo text back",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"text": {"type": "string"}},
                                "required": ["text"],
                            },
                        }
                    ]
                },
            }
        )
    elif method == "tools/call":
        text = req.get("params", {}).get("arguments", {}).get("text", "")
        send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "result": {"content": [{"type": "text", "text": text}]},
            }
        )
    else:
        send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "error": {"code": -32601, "message": f"unknown method {method}"},
            }
        )
