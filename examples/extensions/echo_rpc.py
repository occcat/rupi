#!/usr/bin/env python3
"""JSON-RPC 长连接扩展示例（stdin/stdout 换行帧，对标 rupi-mcp）。

能力：
- initialize 注册斜杠命令 /shout，订阅 tool_call / turn_end
- tools/call 回显 arguments，并带 ui hint
- commands/execute 把参数做成发给模型的 prompt
- 接收 notifications/event（订阅后由宿主推送）
"""
import json
import sys


def reply(mid, result):
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}
    if method == "initialize":
        reply(
            mid,
            {
                "capabilities": {
                    "commands": [
                        {"name": "shout", "description": "uppercase args into a prompt"}
                    ],
                    "events": ["tool_call", "turn_end", "session_start", "session_end"],
                }
            },
        )
    elif method == "tools/call":
        reply(
            mid,
            {
                "content": json.dumps(params.get("arguments") or {}, ensure_ascii=False),
                "ui": {"kind": "note", "message": "echo-rpc handled tools/call"},
            },
        )
    elif method == "commands/execute":
        args = params.get("args") or ""
        reply(mid, {"prompt": f"Please shout this back in ALL CAPS:\n{args}"})
    elif method in ("notifications/event", "notifications/session"):
        continue
    elif mid is not None:
        print(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "error": {"code": -32601, "message": f"method not found: {method}"},
                }
            ),
            flush=True,
        )
