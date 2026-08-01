#!/usr/bin/env bash
# Start/stop the OpenLRLab web app + local tile server (`npm run dev`) as a
# background process, checking first whether either is already running on
# its port.
#
# Usage:
#   scripts/dev.sh start [--port PORT] [--tiles-dir DIR]
#   scripts/dev.sh stop  [--port PORT]
#
#   --port PORT       Vite dev server port (default: 5173)
#   --tiles-dir DIR   start only. Base directory of built PMTiles archives,
#                      passed to the tile server as OPENLR_TILES_DIR
#                      (default: /Users/dave/projects/maps/pmtiles)
#
# The tile server itself always listens on 5176 (see vite.config.js;
# TILE_SERVER_PORT is not currently configurable) but this script checks,
# and on `start` offers to clear, and on `stop` always clears, that port
# too, since it's started by the same `npm run dev` process.
#
# `start` launches the server in the background and returns once it's
# actually answering on $PORT; logs go to web/.dev/server.log.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB_DIR="$(dirname "$SCRIPT_DIR")"
STATE_DIR="$WEB_DIR/.dev"
PID_FILE="$STATE_DIR/server.pid"
LOG_FILE="$STATE_DIR/server.log"

PORT=5173
TILES_DIR="/Users/dave/projects/maps/pmtiles"
TILE_SERVER_PORT=5176

usage() {
  awk '/^#!/{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "$0"
}

CMD="${1:-}"
if [[ $# -gt 0 ]]; then shift; fi

case "$CMD" in
  start|stop) ;;
  -h|--help|"")
    usage; exit 0 ;;
  *)
    echo "Unknown command: $CMD" >&2
    usage
    exit 1 ;;
esac

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      PORT="$2"; shift 2 ;;
    --tiles-dir)
      TILES_DIR="$2"; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1 ;;
  esac
done

port_pid() { lsof -ti:"$1" -sTCP:LISTEN 2>/dev/null || true; }

# Kills whatever is listening on a port and waits for it to actually release
# it. No confirmation — callers decide whether to ask first.
stop_port() {
  local port="$1" label="$2"
  local pid; pid="$(port_pid "$port")"
  if [[ -z "$pid" ]]; then
    echo "$label: nothing listening on port $port."
    return 0
  fi
  kill "$pid"
  for _ in $(seq 1 20); do
    [[ -z "$(port_pid "$port")" ]] && break
    sleep 0.25
  done
  if [[ -n "$(port_pid "$port")" ]]; then
    echo "$label: port $port still in use after SIGTERM." >&2
    return 1
  fi
  echo "$label: stopped (was pid $pid)."
}

# Interactive: if something's already listening, show it and ask before
# killing. Aborts the script on decline.
confirm_and_stop() {
  local port="$1" label="$2"
  local pid; pid="$(port_pid "$port")"
  [[ -z "$pid" ]] && return 0

  echo "$label already running on port $port (pid $pid):"
  lsof -i:"$port" -sTCP:LISTEN | sed 's/^/  /'
  read -r -p "Terminate it and continue? [y/N] " reply
  case "$reply" in
    [yY]|[yY][eE][sS])
      stop_port "$port" "$label" ;;
    *)
      echo "Leaving it running — aborting startup." >&2
      exit 1 ;;
  esac
}

do_start() {
  confirm_and_stop "$PORT" "Vite dev server"
  confirm_and_stop "$TILE_SERVER_PORT" "Tile server"

  if [[ ! -d "$TILES_DIR" ]]; then
    echo "Warning: tiles directory '$TILES_DIR' does not exist — the tile server will 404 on every request." >&2
  fi

  mkdir -p "$STATE_DIR"
  echo "Starting dev server: webapp on port $PORT, tiles from $TILES_DIR"
  (
    cd "$WEB_DIR"
    OPENLR_TILES_DIR="$TILES_DIR" nohup npm run dev -- --port "$PORT" > "$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
  )
  disown -a 2>/dev/null || true

  for _ in $(seq 1 40); do
    curl -sf "http://localhost:$PORT" >/dev/null 2>&1 && break
    sleep 0.25
  done
  if ! curl -sf "http://localhost:$PORT" >/dev/null 2>&1; then
    echo "Server did not come up within 10s — check $LOG_FILE" >&2
    exit 1
  fi

  echo "Running (pid $(cat "$PID_FILE"))."
  echo "  Webapp: http://localhost:$PORT/"
  echo "  Tiles:  http://localhost:$TILE_SERVER_PORT/"
  echo "  Logs:   $LOG_FILE"
  echo "Stop with: $(basename "$0") stop"
}

do_stop() {
  stop_port "$PORT" "Vite dev server"
  stop_port "$TILE_SERVER_PORT" "Tile server"
  rm -f "$PID_FILE"
}

case "$CMD" in
  start) do_start ;;
  stop)  do_stop ;;
esac
