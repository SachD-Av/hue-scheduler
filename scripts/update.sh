#!/bin/sh
# Downloads the latest prebuilt binary from GitHub instead of compiling it locally, which can
# exhaust the memory of a Raspberry Pi, and restarts the service.
set -e

REPO="SachD-Av/hue-scheduler"
DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$DIR/target/release/hue-scheduler"

case "$(uname -m)" in
  aarch64 | arm64) ARCH=aarch64 ;;
  armv7l | armv6l) ARCH=armv7l ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$(dirname "$BIN")"
echo "Downloading hue-scheduler-$ARCH..."
curl -fL --retry 3 -o "$BIN.new" \
  "https://github.com/$REPO/releases/download/latest/hue-scheduler-$ARCH"
chmod +x "$BIN.new"

[ -f "$BIN" ] && cp "$BIN" "$BIN.old"
mv "$BIN.new" "$BIN"
echo "Installed to $BIN (previous version kept as $BIN.old)"

if systemctl cat hue-scheduler >/dev/null 2>&1; then
  systemctl restart hue-scheduler
  echo "Service restarted"
fi
