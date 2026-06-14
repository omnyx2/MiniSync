#!/bin/bash
# docker-sync.sh — Host ↔ Docker container folder sync via minisync
#
# Usage:
#   ./docker-sync.sh [host_folder] [container_name]
#
# Default:
#   host folder:  ~/minisync-shared
#   container:    minisync-container

set -e

HOST_FOLDER="${1:-$HOME/minisync-shared}"
CONTAINER_NAME="${2:-minisync-container}"
HOST_PORT=9100
CONTAINER_PORT=9200
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== minisync: Host ↔ Docker shared folder ==="
echo "Host folder:  $HOST_FOLDER"
echo "Container:    $CONTAINER_NAME"
echo ""

# Create host folder
mkdir -p "$HOST_FOLDER"

# Build Docker image
echo "[1/3] Building Docker image..."
docker build -t minisync "$SCRIPT_DIR" -q

# Start container
echo "[2/3] Starting container..."
docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
docker run -d \
  --name "$CONTAINER_NAME" \
  -p "$CONTAINER_PORT:$CONTAINER_PORT" \
  minisync \
  /data "0.0.0.0:$CONTAINER_PORT"

# Wait for container to be ready
sleep 2

# Get container IP for host→container connection
CONTAINER_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$CONTAINER_NAME" 2>/dev/null || echo "")

echo "[3/3] Starting host sync..."
echo ""
echo "Container is running. Starting host minisync..."
echo "  Host folder:      $HOST_FOLDER"
echo "  Host listening:   0.0.0.0:$HOST_PORT"
echo "  Container peer:   localhost:$CONTAINER_PORT"
echo ""
echo "Files placed in $HOST_FOLDER will sync to the container's /data"
echo "Files placed in container /data will sync back to the host"
echo ""
echo "To put a file into the container:"
echo "  docker exec $CONTAINER_NAME sh -c 'echo hello > /data/test.txt'"
echo ""
echo "To check container files:"
echo "  docker exec $CONTAINER_NAME ls /data"
echo ""
echo "Press Ctrl+C to stop"
echo ""

# Run host minisync (foreground)
"$SCRIPT_DIR/dist/minisync-macos" "$HOST_FOLDER" "0.0.0.0:$HOST_PORT" "localhost:$CONTAINER_PORT"
