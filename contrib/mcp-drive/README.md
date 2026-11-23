# mcp-drive — session driver for a persistent `diskghost mcp` daemon

`diskghost mcp` speaks JSON-RPC on stdin/stdout and exits when stdin
closes — which kills every queued scan. This driver keeps one daemon
alive across agent turns: requests go into a FIFO, results stream to a
JSONL file.

## Layout

```
$DG_MCP_RUN_DIR/          # default ~/.local/state/dg-mcp/run
  in                      # FIFO — daemon stdin
  out.jsonl               # daemon stdout, one JSON object per line
  err.log                 # daemon stderr
  transitions.log         # monitor.sh state changes (TAB-separated)
  jobs-now.tsv            # last poll snapshot
```

Stand the daemon up once:

```bash
RUN_DIR=~/.local/state/dg-mcp/run
mkdir -p "$RUN_DIR" && mkfifo -m600 "$RUN_DIR/in" 2>/dev/null || true
exec 7<>"$RUN_DIR/in"          # hold the FIFO open both ways
diskghost mcp <"$RUN_DIR/in" >>"$RUN_DIR/out.jsonl" 2>>"$RUN_DIR/err.log" &
```

Drive it:

```bash
python3 client.py initialize
python3 client.py tools/call '{"name":"start_scan","arguments":{"path":"/media/x9 pro"}}'
DG_MCP_RUN_DIR=$RUN_DIR ./monitor.sh 300 &   # transition log, TAB-safe
```

## Hard rules (learned the expensive way — diskghost-src#1378)

- **Never** put `$DG_MCP_RUN_DIR` under a scan target: the daemon's own
  `out.jsonl` would be indexed by the scan it runs. If it must live on a
  scanned volume, add the run dir to that location's `exclude`.
- Fields between driver components are TAB- or JSON-delimited, never
  space-separated — mount names contain spaces (`X9 Pro1`).
- The FIFO write is non-blocking: a dead daemon errors in milliseconds
  instead of hanging the caller inside `open()`.

## Tests

```
python3 -m unittest test_client.py -v
```
