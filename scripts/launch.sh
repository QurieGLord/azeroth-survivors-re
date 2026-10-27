#!/usr/bin/env bash
set -euo pipefail

# Find repository root
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

HOST_PORT="8090"
HOST_URL="http://127.0.0.1:${HOST_PORT}/"
STARTED_HOST=0
HOST_PID=""

cleanup() {
    if [ "${STARTED_HOST}" -eq 1 ] && [ -n "${HOST_PID}" ]; then
        echo "Stopping wenilla-host (PID ${HOST_PID})..."
        kill "${HOST_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Ensure wasm bundle is built
if [ ! -f "web/dist/wenilla_bg.wasm" ]; then
    echo "Game wasm not found. Building..."
    make build
fi

# Check if wenilla-host is already running
if ! curl -s --connect-timeout 1 "${HOST_URL}" >/dev/null 2>&1; then
    echo "Starting wenilla-host on ${HOST_URL}..."
    if [ -x "target/release/wenilla-host" ]; then
        ./target/release/wenilla-host --www web/dist --data wow-data/Data &
        HOST_PID=$!
    else
        cargo run --release -p wenilla-host -- --www web/dist --data wow-data/Data &
        HOST_PID=$!
    fi
    STARTED_HOST=1

    # Wait for wenilla-host to respond
    for i in $(seq 1 30); do
        if curl -s --connect-timeout 1 "${HOST_URL}" >/dev/null 2>&1; then
            break
        fi
        sleep 0.2
    done
fi

echo "Launching game in standalone app window..."
CHROME_FLAGS=(
    "--app=${HOST_URL}"
    "--user-data-dir=${HOME}/.local/share/azeroth-survivors-profile"
    "--no-first-run"
    "--no-default-browser-check"
    "--ozone-platform=x11"
    "--enable-features=Vulkan"
    "--use-vulkan"
    "--enable-unsafe-webgpu"
    "--ignore-gpu-blocklist"
    "--enable-gpu-rasterization"
    "--enable-zero-copy"
    "--test-type"
    "--window-size=1600,900"
)

if command -v google-chrome >/dev/null 2>&1; then
    google-chrome "${CHROME_FLAGS[@]}"
elif command -v chromium >/dev/null 2>&1; then
    chromium "${CHROME_FLAGS[@]}"
elif command -v chromium-browser >/dev/null 2>&1; then
    chromium-browser "${CHROME_FLAGS[@]}"
elif command -v brave-browser >/dev/null 2>&1; then
    brave-browser "${CHROME_FLAGS[@]}"
else
    xdg-open "${HOST_URL}"
fi
