#!/usr/bin/env bash
# Claude: checks calibration silently on a temporary virtual device, whose
# input plays back what plays to it:
#   1. the device calibrated against itself, given a wrong latency offset,
#      recovers its true offset, 0, within 2 ms, and again with that offset;
#   2. a private shairport-sync receiver, played onto the device by
#      examples/shairport_to_device at the times it plans, calibrated against
#      the device, then again with the recommended offset: within 2 ms. It
#      covers AirPlay 1 only: shairport-sync takes no AirPlay 2 stream timed
#      by NTP, as spaghetti's are, and the check prints what each session
#      spoke;
#   3. Ctrl-C ends a calibration of the device, and 4. one of the receiver,
#      both timed.
# Nothing plays on a real output, no real microphone records, and no stream
# goes to any receiver but the private one.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/calibration-check.XXXXXX")
name="spaghetti-calibration-check-$$"
uid=""
receiver_pid=""
helper_pid=""

stop_receiver() {
    for pid in "$receiver_pid" "$helper_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    receiver_pid=""
    helper_pid=""
}

cleanup() {
    stop_receiver
    if [[ -n "$uid" ]] && spaghetti --json devices | grep -q "\"$uid\""; then
        spaghetti devices destroy "$uid" >/dev/null
    fi
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

free_port() {
    python3 -I -c 'import socket; s = socket.socket(); s.bind(("", 0)); print(s.getsockname()[1])'
}

cargo build --quiet --release --manifest-path "$root/Cargo.toml" -p spaghetti
cargo build --quiet --release --manifest-path "$root/Cargo.toml" -p spaghetti-engine \
    --example shairport_to_device
cli="$root/target/release/spaghetti"
helper="$root/target/release/examples/shairport_to_device"

uid=$(spaghetti --json devices create "$name" \
    | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["uid"])')

# Claude: prints "late_ms of the output, late_ms of the reference, offset",
# and on standard error both spreads, any error or warning, and the protocol
# and timing an AirPlay session spoke.
measure() {
    local status=0
    RUST_LOG=warn,airplay=info "$cli" --json calibrate "$@" --microphone "$uid" \
        > "$work/calibrate.json" 2> "$work/calibrate.log" || status=$?
    grep -E "^error|WARN" "$work/calibrate.log" >&2 || true
    grep -o 'protocol=AirPlay [12] timing="[A-Z]*"' "$work/calibrate.log" | sed "s/^/spoke: /" >&2 || true
    [[ $status -eq 0 ]] || return "$status"
    python3 -I -c '
import json, sys
r = json.load(sys.stdin)
print("spreads: output {:.4f} ms, reference {:.4f} ms".format(
    r["output"]["spread_ms"], r["reference"]["spread_ms"]), file=sys.stderr)
print(r["output"]["late_ms"], r["reference"]["late_ms"], r["latency_offset_ms"])' < "$work/calibrate.json"
}

# Claude: fails unless |$1 - $2| <= $3, labelled $4.
within() {
    python3 -I -c 'import sys; a, b, bound = map(float, sys.argv[1:4]); sys.exit(abs(a - b) > bound)' "$1" "$2" "$3" \
        || { echo "FAIL: $4: $1 against $2" >&2; exit 1; }
}

start_receiver() {
    local port udp
    port=$(free_port)
    udp=$((20000 + RANDOM % 30000))
    cat > "$work/shairport-sync.conf" <<EOF
general = {
  name = "$name";
  port = $port;
  udp_port_base = $udp;
  udp_port_range = 10;
  output_backend = "stdout";
  mdns_backend = "dns-sd";
  ignore_volume_control = "yes";
};
diagnostics = {
  log_verbosity = 3;
};
EOF
    rm -f "$work/pcm" "$work/shairport-sync.log" "$work/helper.log"
    : > "$work/shairport-sync.log"
    mkfifo "$work/pcm"
    "$helper" "$uid" "$work/shairport-sync.log" < "$work/pcm" 2> "$work/helper.log" &
    helper_pid=$!
    shairport-sync -c "$work/shairport-sync.conf" > "$work/pcm" 2> "$work/shairport-sync.log" &
    receiver_pid=$!
    for _ in $(seq 100); do
        grep -q "playing on $uid" "$work/helper.log" 2>/dev/null && break
        kill -0 "$helper_pid" 2>/dev/null || { cat "$work/helper.log" >&2; exit 1; }
        sleep 0.1
    done
    grep -q "playing on $uid" "$work/helper.log" || { echo "FAIL: the helper did not open $uid" >&2; exit 1; }
    for _ in $(seq 30); do
        receiver=$(spaghetti --json airplay | python3 -I -c '
import json, sys
name = sys.argv[1]
receivers = json.load(sys.stdin)
mine = [r["id"] for r in receivers if r["name"] == name]
if len(mine) == 1 and sum(r["id"] == mine[0] for r in receivers) == 1:
    print(mine[0])' "$name")
        [[ -n "$receiver" ]] && return
        sleep 1
    done
    echo "FAIL: receiver $name not found once by a unique id" >&2
    exit 1
}

# Claude: runs a calibration with $@, interrupts it after $1 s, and prints how
# long it took to exit after that, failing unless it exited as cancelled.
interrupted() {
    local after=$1
    shift
    python3 -I -c '
import signal, subprocess, sys, time
after, command = float(sys.argv[1]), sys.argv[2:]
calibration = subprocess.Popen(command, stdout=subprocess.DEVNULL)
time.sleep(after)
if calibration.poll() is not None:
    sys.exit(f"FAIL: the calibration ended before it was interrupted, with {calibration.returncode}")
sent = time.monotonic()
calibration.send_signal(signal.SIGINT)
status = calibration.wait()
print(f"{(time.monotonic() - sent) * 1000:.0f}")
if status != 130:
    sys.exit(f"FAIL: exited {status}, not 130")' "$after" "$cli" --json calibrate "$@" --microphone "$uid"
}

echo "== 1. device against itself, given a 25 ms wrong offset"
read -r out ref offset < <(measure "$uid" --reference "$uid" --delay 300 --latency-offset 25)
echo "output $out ms, reference $ref ms, recommended --latency-offset $offset"
within "$offset" 0 2 "recommended offset"
read -r out ref again < <(measure "$uid" --reference "$uid" --delay 300 --latency-offset "$offset")
echo "with $offset: output $out ms, reference $ref ms, recommended --latency-offset $again"
within "$out" "$ref" 2 "residuals with the recommended offset"

echo "== 2. private shairport-sync receiver against the device"
start_receiver
echo "receiver $name is $receiver"
read -r out ref offset < <(measure "airplay://$receiver" --reference "$uid" --delay 3000)
echo "output $out ms, reference $ref ms, recommended --latency-offset $offset"
stop_receiver
start_receiver
read -r out ref again < <(measure "airplay://$receiver" --reference "$uid" --delay 3000 --latency-offset "$offset")
echo "with $offset: output $out ms, reference $ref ms, recommended --latency-offset $again"
within "$out" "$ref" 2 "receiver residuals with the recommended offset"
stop_receiver

echo "== 3. Ctrl-C while probing the device"
elapsed=$(interrupted 3 "$uid" --reference "$uid" --delay 300)
echo "exited $elapsed ms after Ctrl-C"
within "$elapsed" 0 1000 "time to exit"

echo "== 4. Ctrl-C while streaming to the receiver"
start_receiver
elapsed=$(interrupted 6 "airplay://$receiver" --reference "$uid" --delay 3000)
echo "exited $elapsed ms after Ctrl-C"
within "$elapsed" 0 1500 "time to exit"
grep -q '"TEARDOWN"\|TEARDOWN' "$work/shairport-sync.log" || { echo "FAIL: no TEARDOWN reached the receiver" >&2; exit 1; }
stop_receiver

echo "PASS"
