#!/usr/bin/env python3
"""最小 fake MCP server：stdio 上换行分隔 JSON-RPC 2.0。

实现 initialize / notifications/initialized / tools/list / tools/call(echo, fail) /
resources/list + resources/read（单个静态文本资源），
供 rupi-mcp 集成测试做真实子进程联调。
"""

import json
import sys

# server 收到的 roots/list 应答存在这里，roots_probe 工具读出来给测试断言
seen_roots = None


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    global seen_roots
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        # 无 method 即对我方请求的应答（roots/list 回包）：记下来，不报错
        if "method" not in req:
            if req.get("id") == 9001:
                seen_roots = req.get("result", {}).get("roots")
            continue
        if "id" not in req:
            # notification：initialized 到达即反向请求 roots/list，验证桥会应答
            if req.get("method") == "notifications/initialized":
                send({"jsonrpc": "2.0", "id": 9001, "method": "roots/list", "params": {}})
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
                            {
                                "name": "roots_probe",
                                "description": "returns roots/list answers seen from client",
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
            elif name == "roots_probe":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {
                            "content": [
                                {"type": "text", "text": json.dumps(seen_roots)}
                            ]
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
        elif method == "resources/list":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {
                        "resources": [
                            {
                                "uri": "test://notes/hello",
                                "name": "hello-notes",
                                "mimeType": "text/plain",
                            }
                        ]
                    },
                }
            )
        elif method == "resources/read":
            uri = req.get("params", {}).get("uri", "")
            if uri == "test://notes/hello":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {
                            "contents": [
                                {
                                    "uri": uri,
                                    "mimeType": "text/plain",
                                    "text": "HELLO-RESOURCE-CONTENT",
                                }
                            ]
                        },
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": {"code": -32602, "message": "unknown resource"},
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
