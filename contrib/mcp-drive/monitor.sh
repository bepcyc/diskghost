#!/bin/bash
# Polls list_jobs every ${1:-300}s, appends state-transition lines to
# transitions.log in $DG_MCP_RUN_DIR. Fields are TAB-separated so a
# target like "/media/user/My Disk 1" never corrupts a record
# (diskghost-src#1378: space-separated fields split the path mid-word).
set -u
RUN_DIR="${DG_MCP_RUN_DIR:-$HOME/.local/state/dg-mcp/run}"
cd "$(dirname "$0")"
declare -A LAST
while true; do
  python3 - <<'PY' > "$RUN_DIR/jobs-now.tsv" 2>/dev/null
import sys; sys.path.insert(0,'.')
from client import call
r=call('tools/call',{'name':'list_jobs','arguments':{}},wait=60)
for j in (r.get('result') or {}).get('structuredContent',{}).get('jobs',[]):
    print(j['job_id'],j['state'],j.get('phase',''),j['target'],j['files'],j['bytes'],sep='\t')
PY
  while IFS=$'\t' read -r jid state phase target files bytes; do
    key="$jid:$state:$phase"
    if [ "${LAST[$jid]:-}" != "$key" ]; then
      echo "$(date +%H:%M:%S)	$jid	$state	$phase	$target	files=$files	bytes=$bytes" >> "$RUN_DIR/transitions.log"
      LAST[$jid]="$key"
    fi
  done < "$RUN_DIR/jobs-now.tsv"
  sleep "${1:-300}"
done
