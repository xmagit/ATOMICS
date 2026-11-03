#!/usr/bin/env bash
# The desktop's QA screenshot of the web client's fake host, as near as ctl gets it, for the wide browser
# test to put beside its own (crates/atomics-web/tests/browser.rs): a plan "Parse faster" with t1 running and
# t2 after it, the board in the right panel, one agent's tile, and the chief docked. The image has no agent CLI,
# so a shell first installs a stand-in `claude` that runs `sh`, which the chief starts. Run it through
# `atomics queue --class qa`; it writes target/wide-shots/desktop.png, or the file given.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/target/wide-shots/desktop.png}
stand_in="printf '#!/bin/sh\\\\nexec sh\\\\n' > /usr/local/bin/claude; chmod +x /usr/local/bin/claude; PS1='\$ '; clear\\r"
exec "$root/scripts/linux-qa.sh" screenshot "$out" --size 1600x1000 \
    --ctl '{"cmd":"close","session":"project"}' \
    --ctl '{"cmd":"start","agent":"shell","isolation":"bare"}' \
    --ctl "{\"cmd\":\"send\",\"session\":\"new session\",\"raw\":true,\"text\":\"$stand_in\"}" \
    --ctl '{"cmd":"plan_set","goal":"Parse faster","tasks":[{"id":"t1","title":"Task t1","status":"running","agent":"claude","branch":"atomics/t1"},{"id":"t2","title":"Task t2","depends_on":["t1"],"agent":"claude"}]}' \
    --ctl '{"cmd":"activate","session":"new session"}' \
    --ctl '{"cmd":"show_chief","open":true,"dock":"docked"}' \
    --ctl '{"cmd":"show_diff","open":true}'
