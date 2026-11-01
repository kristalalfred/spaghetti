#!/usr/bin/env bash
# Claude: creates a spaghetti virtual device through the running daemon, runs
# the engine's input checks against it, the last of which removes it, and
# removes it if a check failed first. Nothing plays audibly.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
uid=""

cleanup() {
    if [[ -n "$uid" ]] && spaghetti --json devices | grep -q "\"$uid\""; then
        spaghetti devices destroy "$uid" >/dev/null
    fi
}
trap cleanup EXIT INT TERM

uid=$(spaghetti --json devices create "spaghetti-input-check-$$" \
    | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["uid"])')
cd "$root"
for check in a_loopback_input_dates_each_burst_when_its_route_played_it \
    an_input_device_that_goes_away_ends_its_source_for_good; do
    SPAGHETTI_LOOPBACK_UID="$uid" cargo test -p spaghetti-engine --lib -- \
        --ignored --exact --nocapture "input::tests::$check"
done
