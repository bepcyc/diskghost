"""Tests for contrib/mcp-drive (diskghost-src#1378): no deps, `python3 -m unittest`."""

import json
import os
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import client


class FifoCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.run = self.tmp.name
        client.IN = os.path.join(self.run, "in")
        client.OUT = os.path.join(self.run, "out.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def test_no_reader_fails_fast_not_hangs(self):
        # A FIFO with no reader must return an error near-instantly: the old
        # client blocked in open() and its 30s timeout never applied.
        os.mkfifo(client.IN)
        t0 = time.time()
        r = client.call("tools/list", wait=30)
        self.assertLess(time.time() - t0, 5)
        self.assertIn("error", r)

    def test_target_with_spaces_round_trips(self):
        os.mkfifo(client.IN)
        seen = {}

        def fake_daemon():
            with open(client.IN) as f:
                req = json.loads(f.readline())
            seen["req"] = req
            target = req["params"]["arguments"]["path"]
            resp = {"jsonrpc": "2.0", "id": req["id"],
                    "result": {"structuredContent": {"echoed": target}}}
            with open(client.OUT, "a") as out:
                out.write(json.dumps(resp) + "\n")

        threading.Thread(target=fake_daemon, daemon=True).start()
        path = "/media/bepcyc/X9 Pro1"
        r = client.call(
            "tools/call",
            {"name": "add_location", "arguments": {"path": path}},
            wait=5,
        )
        echoed = r["result"]["structuredContent"]["echoed"]
        self.assertEqual(echoed, path)
        self.assertEqual(seen["req"]["params"]["arguments"]["path"], path)

    def test_timeout_when_no_response(self):
        os.mkfifo(client.IN)
        # O_RDWR holds the FIFO open both ways without blocking: a reader
        # exists for the client's nonblocking write, but nothing answers.
        fd = os.open(client.IN, os.O_RDWR)
        try:
            t0 = time.time()
            r = client.call("tools/list", wait=2)
        finally:
            os.close(fd)
        self.assertEqual(r["error"], "timeout")
        self.assertLess(time.time() - t0, 6)


if __name__ == "__main__":
    unittest.main()
