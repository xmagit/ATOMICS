#!/bin/bash
# Container-side operations; called by linux-qa.sh with an isolated environment.
set -euo pipefail

qa=$(mktemp -d /tmp/imacs-linux-qa.XXXXXX)
app_pid=
cleanup() {
    if [[ -n $app_pid ]]; then
        # setsid below gives Xvfb, D-Bus, IMACS and its shell one process group.
        kill -- "-$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
    fi
    rm -rf "$qa"
}
trap cleanup EXIT
export IMACS_HOME="$qa/state" IMACS_CONFIG="$qa/config.toml" IMACS_SOCKET="$qa/imacs.sock"
export XDG_RUNTIME_DIR="$qa/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
unset IMACS_PANE WAYLAND_DISPLAY

case "${1:-}" in
    test)
        python3 /repo/tests/test_linux_qa.py
        xvfb-run -a -s '-screen 0 1600x1000x24 -nolisten tcp' \
            cargo test --workspace --locked --no-fail-fast
        ;;
    screenshot)
        size=$2
        shift 2
        # Keep build diagnostics away from stdout, which carries only PNG bytes.
        cargo build --locked -p imacs >&2
        mkdir "$qa/project"
        git -C "$qa/project" init -q -b main
        echo '# IMACS Linux QA' > "$qa/project/README.md"
        git -C "$qa/project" add README.md
        git -C "$qa/project" commit -qm 'Create isolated QA project'
        # Xvfb owns its display allocation and dbus-run-session owns its bus.
        setsid xvfb-run -a -s "-screen 0 ${size}x24 -nolisten tcp -dpi 96" \
            dbus-run-session -- "$CARGO_TARGET_DIR/debug/imacs" "$qa/project" \
            --size "$size" --open "shell:$qa/project" > "$qa/app.log" 2>&1 &
        app_pid=$!
        ready=false
        deadline=$((SECONDS + 30))
        while (( SECONDS < deadline )); do
            if ! kill -0 "$app_pid" 2>/dev/null; then
                cat "$qa/app.log" >&2
                echo 'Linux IMACS exited before its control socket was ready.' >&2
                exit 1
            fi
            if [[ -S $IMACS_SOCKET ]] && timeout 5 "$CARGO_TARGET_DIR/debug/imacs" ctl '{"cmd":"list"}' > "$qa/ready.json" 2>/dev/null \
                && python3 -c 'import json,sys; sys.exit(not json.load(open(sys.argv[1])).get("ok"))' "$qa/ready.json"; then
                ready=true
                break
            fi
            sleep 0.1
        done
        if [[ $ready != true ]]; then
            cat "$qa/app.log" >&2
            echo 'Timed out waiting for the Linux IMACS control socket.' >&2
            exit 1
        fi
        # Requests from --ctl set the scene, in order; each must succeed.
        for request in "$@"; do
            if ! timeout 30 "$CARGO_TARGET_DIR/debug/imacs" ctl "$request" > "$qa/ctl.json" \
                || ! python3 -c 'import json,sys; sys.exit(not json.load(open(sys.argv[1])).get("ok"))' "$qa/ctl.json"; then
                cat "$qa/ctl.json" "$qa/app.log" >&2
                echo "Control request failed: $request" >&2
                exit 1
            fi
        done
        if [[ $# -gt 0 ]]; then
            # Let new terminals draw their first output before the capture.
            sleep 2
        fi
        if ! timeout 30 "$CARGO_TARGET_DIR/debug/imacs" ctl \
            "{\"cmd\":\"screenshot\",\"path\":\"$qa/screenshot.png\"}" > "$qa/screenshot.json"; then
            cat "$qa/app.log" >&2
            exit 1
        fi
        python3 - "$qa/screenshot.json" "$qa/screenshot.png" "$size" <<'PY'
import json
import struct
import sys

response = json.load(open(sys.argv[1]))
if not response.get("ok"):
    sys.exit(f"Linux screenshot failed: {response}")
with open(sys.argv[2], "rb") as screenshot:
    header = screenshot.read(24)
expected = tuple(map(int, sys.argv[3].split("x")))
if header[:8] != b"\x89PNG\r\n\x1a\n" or struct.unpack(">II", header[16:24]) != expected:
    sys.exit(f"Expected a PNG of size {expected}, got {header!r}")
PY
        cat "$qa/screenshot.png"
        ;;
    *) echo 'Use scripts/linux-qa.sh on the host.' >&2; exit 2 ;;
esac
