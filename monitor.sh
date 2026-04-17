#!/usr/bin/env bash
# Monitor lightorrent downloads and append a CSV row per torrent on each poll.
# Requires LIGHTORRENT_HOST and LIGHTORRENT_PASS; optional LIGHTORRENT_USER,
# LIGHTORRENT_INTERVAL, LIGHTORRENT_CSV.
set -euo pipefail

HOST="${LIGHTORRENT_HOST:-}"
USER="${LIGHTORRENT_USER:-admin}"
PASS="${LIGHTORRENT_PASS:-}"
CSV="${LIGHTORRENT_CSV:-torrent-stats.csv}"
INTERVAL="${LIGHTORRENT_INTERVAL:-15}"

if [[ -z "$HOST" ]]; then
  echo "Set LIGHTORRENT_HOST (e.g. 'host:8181')" >&2
  exit 1
fi
if [[ -z "$PASS" ]]; then
  echo "Set LIGHTORRENT_PASS" >&2
  exit 1
fi

# Login
SID=$(curl -s -c - "http://$HOST/api/v2/auth/login" \
  -d "username=$USER&password=$PASS" 2>/dev/null | grep SID | awk '{print $NF}')

if [[ -z "$SID" ]]; then
  echo "Login failed" >&2
  exit 1
fi
echo "Logged in (SID: ${SID:0:4}...)"

# CSV header
if [[ ! -f "$CSV" ]]; then
  echo "timestamp,name,hash,state,progress,dl_speed_bytes,ul_speed_bytes,downloaded,uploaded,ratio,size,eta,num_seeds,num_leeches" > "$CSV"
fi

echo "Monitoring → $CSV (every ${INTERVAL}s). Ctrl+C to stop."

while true; do
  ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  data=$(curl -s -b "SID=$SID" "http://$HOST/api/v2/torrents/info" 2>/dev/null)

  count=$(echo "$data" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d))" 2>/dev/null || echo 0)

  if [[ "$count" == "0" ]]; then
    echo "[$ts] No torrents"
  else
    echo "$data" | python3 -c "
import sys, json, csv, io
torrents = json.load(sys.stdin)
ts = '$ts'
for t in torrents:
    row = [
        ts,
        t.get('name','?'),
        t.get('hash','?'),
        t.get('state','?'),
        f\"{t.get('progress',0)*100:.1f}%\",
        t.get('dlspeed',0),
        t.get('upspeed',0),
        t.get('downloaded',0),
        t.get('uploaded',0),
        f\"{t.get('ratio',0):.3f}\",
        t.get('size',0),
        t.get('eta',0),
        t.get('num_seeds',0),
        t.get('num_leechs',0),
    ]
    w = csv.writer(sys.stdout)
    w.writerow(row)
    # Also print human-readable
    name = t.get('name','?')[:40]
    prog = t.get('progress',0)*100
    dl = t.get('dlspeed',0) / 1024 / 1024
    ul = t.get('upspeed',0) / 1024 / 1024
    print(f'  {name:<40} {prog:5.1f}%  ↓{dl:.1f} MB/s  ↑{ul:.1f} MB/s', file=sys.stderr)
" >> "$CSV"
  fi

  sleep "$INTERVAL"
done
