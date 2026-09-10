#!/usr/bin/env python3
"""最小 fake MCP server：stdio 上换行分隔 JSON-RPC 2.0。

实现 initialize / notifications/initialized / tools/list / tools/call(echo, fail) /
resources/list + resources/read（单个静态文本资源）/
prompts/list + prompts/get（单个带参模板），
供 rupi-mcp 集成测试做真实子进程联调。
"""

import json
import os
import sys

# server 收到的 roots/list 应答存在这里，roots_probe 工具读出来给测试断言
seen_roots = None

# 动态工具门（list_changed 联调专用）：FAKE_MCP_DYNAMIC=1 时 tools/list 按调用计数变脸——
# 第 1 次回基础 3 件套并附 notifications/tools/list_changed，第 2 次多出 late，第 3 次起
# late 消失（再附一次通知）。默认关闭，既有断言（tools.len()==3）不受影响。
DYNAMIC = os.environ.get("FAKE_MCP_DYNAMIC") == "1"
list_count = 0


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    global seen_roots, list_count
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
            list_count += 1
            tools = [
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
            if DYNAMIC and list_count == 2:
                tools.append(
                    {
                        "name": "late",
                        "description": "appears after list_changed",
                        "inputSchema": {"type": "object"},
                    }
                )
            send({"jsonrpc": "2.0", "id": rid, "result": {"tools": tools}})
            if DYNAMIC and list_count <= 2:
                send(
                    {
                        "jsonrpc": "2.0",
                        "method": "notifications/tools/list_changed",
                        "params": {},
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
            elif DYNAMIC and name == "late":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {"content": [{"type": "text", "text": "LATE-OK"}]},
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
        elif method == "prompts/list":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": rid,
                    "result": {
                        "prompts": [
                            {
                                "name": "greet",
                                "description": "greet a person by name",
                                "arguments": [
                                    {
                                        "name": "name",
                                        "description": "who to greet",
                                        "required": True,
                                    }
                                ],
                            }
                        ]
                    },
                }
            )
        elif method == "prompts/get":
            params = req.get("params", {})
            if params.get("name") == "greet":
                who = params.get("arguments", {}).get("name", "stranger")
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "result": {
                            "description": "a greeting",
                            "messages": [
                                {
                                    "role": "user",
                                    "content": {
                                        "type": "text",
                                        "text": "Hello, " + str(who) + "!",
                                    },
                                }
                            ],
                        },
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": {"code": -32602, "message": "unknown prompt"},
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
