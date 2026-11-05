#!/usr/bin/env python3
"""Fake MCP server over stdio whose tool list grows mid-session.

It starts with one tool, `probe_add`. Calling it answers, then announces
`notifications/tools/list_changed`; from then on the server also lists
`probe_greet`, whose call answers `GREETED-PROBE-4417`.
"""

import json
import sys

ADD = {
    "name": "probe_add",
    "description": "Adds the probe_greet tool.",
    "inputSchema": {"type": "object", "properties": {}},
}
GREET = {
    "name": "probe_greet",
    "description": "Greets the probe.",
    "inputSchema": {"type": "object", "properties": {}},
}


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def reply(request, result):
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


def text(value):
    return {"content": [{"type": "text", "text": value}], "isError": False}


def main():
    tools = [ADD]
    for line in sys.stdin:
        request = json.loads(line)
        method = request.get("method")
        if "id" not in request:
            continue
        if method == "initialize":
            reply(
                request,
                {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {"listChanged": True}},
                    "serverInfo": {"name": "changing-tools"},
                },
            )
        elif method == "tools/list":
            reply(request, {"tools": tools})
        elif method == "tools/call":
            name = request["params"]["name"]
            if name == "probe_add":
                reply(request, text("ADDED-PROBE-GREET"))
                if GREET not in tools:
                    tools = [ADD, GREET]
                    send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            elif name == "probe_greet" and GREET in tools:
                reply(request, text("GREETED-PROBE-4417"))
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": -32602, "message": f"unknown tool {name}"},
                    }
                )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32601, "message": f"no method {method}"},
                }
            )


if __name__ == "__main__":
    main()
