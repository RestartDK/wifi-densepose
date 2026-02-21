#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIND_ADDR="${WIFI_DENSEPOSE_BIND_ADDR:-127.0.0.1:8787}"
API_BASE="http://${BIND_ADDR}"
DASHBOARD_PORT="${WIFI_DENSEPOSE_DASHBOARD_PORT:-5173}"
DASHBOARD_URL="http://127.0.0.1:${DASHBOARD_PORT}/examples/pose-dashboard.html"
ALLOWED_ORIGINS="${WIFI_DENSEPOSE_ALLOWED_ORIGINS:-http://localhost:${DASHBOARD_PORT},http://127.0.0.1:${DASHBOARD_PORT}}"

SERVER_LOG="${ROOT_DIR}/target/pose-demo-server.log"
WEB_LOG="${ROOT_DIR}/target/pose-demo-web.log"

SERVER_PID=""
WEB_PID=""

cleanup() {
  if [[ -n "${WEB_PID}" ]]; then
    kill "${WEB_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${SERVER_PID}" ]]; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT INT TERM

mkdir -p "${ROOT_DIR}/target"

echo "Starting wifi-densepose-server on ${BIND_ADDR}"
(
  cd "${ROOT_DIR}"
  WIFI_DENSEPOSE_BIND_ADDR="${BIND_ADDR}" \
    WIFI_DENSEPOSE_ALLOWED_ORIGINS="${ALLOWED_ORIGINS}" \
    cargo run -p wifi-densepose-server
) >"${SERVER_LOG}" 2>&1 &
SERVER_PID="$!"

for _ in {1..60}; do
  if curl --silent --show-error --fail "${API_BASE}/healthz" >/dev/null; then
    break
  fi
  sleep 0.25
done

if ! curl --silent --show-error --fail "${API_BASE}/healthz" >/dev/null; then
  echo "Server failed health check. See ${SERVER_LOG}" >&2
  exit 1
fi

curl --silent --show-error --fail \
  --request POST "${API_BASE}/api/v1/pose/demo/seed" \
  --header "content-type: application/json" \
  --data '{"survivors":3}' >/dev/null

echo "Starting static dashboard server on 127.0.0.1:${DASHBOARD_PORT}"
(
  cd "${ROOT_DIR}"
  python3 -m http.server "${DASHBOARD_PORT}" --bind 127.0.0.1
) >"${WEB_LOG}" 2>&1 &
WEB_PID="$!"

if command -v open >/dev/null 2>&1; then
  open "${DASHBOARD_URL}" >/dev/null 2>&1 || true
elif command -v xdg-open >/dev/null 2>&1; then
  xdg-open "${DASHBOARD_URL}" >/dev/null 2>&1 || true
fi

echo "Pose demo is live"
echo "- API: ${API_BASE}"
echo "- Dashboard: ${DASHBOARD_URL}"
echo "- Logs: ${SERVER_LOG}, ${WEB_LOG}"
echo "Press Ctrl+C to stop"

wait "${SERVER_PID}" "${WEB_PID}"
