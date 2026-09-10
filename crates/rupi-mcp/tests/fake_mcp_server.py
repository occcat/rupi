#!/usr/bin/env python3
"""最小 fake MCP server：stdio 上换行分隔 JSON-RPC 2.0。

实现 initialize / notifications/initialized / tools/list / tools/call(echo, fail)，
供 rupi-mcp 集成测试做真实子进程联调。
"""

import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "id" not in req:
            # notification，直接忽略
            continue
        method = req.get("method")
        rid = req.get("id")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": "2024-11-05"}})
        elif method == "tools/list":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {
                        "tools": [
                            {
                                "name": "echo",
                                "description": "echo text back",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "text": {"type": "string"},
                                        "shout": {"type": "boolean"},
                                    },
                                    "required": ["text"],
                                },
                            },
                            {
                                "name": "fail",
                                "description": "always fails as tool error",
                                "inputSchema": {"type": "object"},
                            },
                        ]
                    },
                }
            )
        elif method == "tools/call":
            params = req.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {})
            if name == "echo":
                text = args.get("text", "")
                if args.get("shout") is True:
                    text = str(text).upper()
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {"content": [{"type": "text", "text": text}]},
                    }
                )
            elif name == "fail":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {
                            "content": [{"type": "text", "text": "boom"}],
                            "isError": True,
                        },
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": {"code": -32602, "message": "unknown tool"},
                    }
                )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": {"code": -32601, "message": "method not found"},
                }
            )


if __name__ == "__main__":
    main()
