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
  space-separated — mount names contain spaces (`My Disk 1`).
- The FIFO write is non-blocking: a dead daemon errors in milliseconds
  instead of hanging the caller inside `open()`.

## Mounting SMB-only sources headless (diskghost-src#1377)

`ssh://host//path` is the preferred remote transport; when a source has
no sshd and only an old SMB server (e.g. a set-top box's built-in file
sharing, SMB1-only), do NOT reach for gvfs — it is bound to the desktop
session, dies on logout/reboot, and the product's own docs warn it can
serve a fresh inode per path. The headless equivalent is a kernel cifs
mount:

```bash
# once, as root — credentials live in a root-owned file, never in cmdline
sudo install -m600 /dev/null /etc/diskghost-smb.creds
sudo tee -a /etc/diskghost-smb.creds <<'EOF'
user=<smb-user>
password=<smb-password>
EOF
sudo mkdir -p /mnt/<share-name>
# fstab line for permanence:
echo '//<smb-host>/<share> /mnt/<share-name> cifs vers=1.0,credentials=/etc/diskghost-smb.creds,ro,noauto,x-systemd.automount,x-systemd.idle-timeout=300,uid=1000 0 0' | sudo tee -a /etc/fstab
sudo mount /mnt/<share-name>
```

Then `add_location /mnt/<share-name>` — a stable path with a stable
device id, so the location uuid survives remounts and rescans are
incremental. Without root, an on-device sshd is the only session-free
alternative; gvfs is a session workaround, not a transport.

## Tests

```
python3 -m unittest test_client.py -v
```
