"""Minimal stdio-less MCP driver for a persistent `diskghost mcp` daemon.

The daemon reads JSON-RPC lines from a FIFO and writes result lines to a
JSONL file; this client writes requests and polls the file for the
matching id. Session tooling — not part of the shipped binary.

Defects fixed vs the ad-hoc original (diskghost-src#1378):
- `open(IN, "w")` blocks forever when the daemon is down: the request
  timeout never applied. Opening with O_NONBLOCK fails fast (ENXIO)
  instead of hanging the caller.
- Every path travels inside JSON, never through shell words, so a
  target like `/media/bepcyc/X9 Pro1` round-trips byte-for-byte.
- IN/OUT are module constants a test can repoint; nothing here touches
  a scan target unless RUN_DIR is pointed at one (see README).
"""

import json
import os
import sys
import time

RUN_DIR = os.environ.get("DG_MCP_RUN_DIR", os.path.expanduser("~/.local/state/dg-mcp/run"))
IN = os.path.join(RUN_DIR, "in")
OUT = os.path.join(RUN_DIR, "out.jsonl")


def call(method, params=None, rpc_id=None, wait=30):
    rpc_id = rpc_id or int(time.time() * 1000) % 10**9
    req = {"jsonrpc": "2.0", "id": rpc_id, "method": method}
    if params is not None:
        req["params"] = params
    try:
        fd = os.open(IN, os.O_WRONLY | os.O_NONBLOCK)
    except OSError as e:
        return {"error": f"cannot write FIFO: {e}", "id": rpc_id}
    with os.fdopen(fd, "w") as f:
        f.write(json.dumps(req) + "\n")
    t0 = time.time()
    while time.time() - t0 < wait:
        try:
            with open(OUT) as out:
                for line in out:
                    try:
                        m = json.loads(line)
                    except ValueError:
                        continue
                    if m.get("id") == rpc_id:
                        return m
        except FileNotFoundError:
            pass
        time.sleep(0.3)
    return {"error": "timeout", "id": rpc_id}


if __name__ == "__main__":
    method = sys.argv[1]
    params = json.loads(sys.argv[2]) if len(sys.argv) > 2 else None
    wait = int(sys.argv[3]) if len(sys.argv) > 3 else 30
    print(json.dumps(call(method, params, wait=wait), indent=1))
