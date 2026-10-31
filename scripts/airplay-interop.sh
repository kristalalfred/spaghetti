#!/usr/bin/env bash
# Claude: plays a 10 s test signal through crates/airplay's example to a private
# shairport-sync that writes what it receives to a file, then checks that the
# received audio equals the signal and that shairport-sync plans to play it
# when the stream said. Nothing plays audibly.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
name="spaghetti-airplay-test-$$"
work=$(mktemp -d "${TMPDIR:-/tmp}/airplay-interop.XXXXXX")
receiver_pid=""

cleanup() {
    if [[ -n "$receiver_pid" ]] && kill -0 "$receiver_pid" 2>/dev/null; then
        kill "$receiver_pid" 2>/dev/null || true
        wait "$receiver_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

free_port() {
    python3 -I -c 'import socket; s = socket.socket(); s.bind(("", 0)); print(s.getsockname()[1])'
}

rtsp_port=$(free_port)
udp_base=$((20000 + RANDOM % 30000))
cat > "$work/shairport-sync.conf" <<EOF
general = {
  name = "$name";
  port = $rtsp_port;
  udp_port_base = $udp_base;
  udp_port_range = 10;
  output_backend = "stdout";
  mdns_backend = "dns-sd";
  ignore_volume_control = "yes";
};
diagnostics = {
  log_verbosity = 3;
};
EOF

ffmpeg -nostdin -loglevel error -f lavfi \
    -i "aevalsrc=exprs=0.4*sin(2*PI*441*t)+0.2*(random(0)-0.5)|0.4*sin(2*PI*660*t)+0.2*(random(1)-0.5):s=44100:d=10" \
    -c:a pcm_s16le "$work/source.wav"

cargo build --quiet --release -p airplay --example play --manifest-path "$root/Cargo.toml"

shairport-sync -c "$work/shairport-sync.conf" > "$work/received.pcm" 2> "$work/shairport-sync.log" &
receiver_pid=$!

if ! "$root/target/release/examples/play" "$name" --wav "$work/source.wav" --gain 0 2> "$work/play.log"; then
    cat "$work/play.log" >&2
    tail -n 20 "$work/shairport-sync.log" >&2
    exit 1
fi
sleep 1
kill "$receiver_pid"
wait "$receiver_pid" 2>/dev/null || true
receiver_pid=""

python3 -I - "$work/source.wav" "$work/received.pcm" "$work/shairport-sync.log" "$work/play.log" <<'EOF'
import array
import re
import sys

source_path, received_path, log_path, play_log_path = sys.argv[1:]
wav = open(source_path, "rb").read()
at = 12
while wav[at:at + 4] != b"data":
    at += 8 + int.from_bytes(wav[at + 4:at + 8], "little")
size = int.from_bytes(wav[at + 4:at + 8], "little")
source = wav[at + 8:at + 8 + size]
received = open(received_path, "rb").read()

offset = received.find(source[:1024])
while offset >= 0 and offset % 4:
    offset = received.find(source[:1024], offset + 1)
frames = len(source) // 4
if offset < 0:
    sys.exit(f"FAIL: the first frames of the signal never arrived ({len(received) // 4} frames received)")

window = received[offset:offset + len(source)]
expected = array.array("h", source)
actual = array.array("h", window)
if len(window) == len(source) and window == source:
    print(f"PASS: all {frames} frames arrived bit-exact, after {offset // 4} frames of lead-in")
else:
    mismatched = [i for i, (a, b) in enumerate(zip(expected, actual)) if a != b]
    missing = len(expected) - len(actual)
    first = f", first at frame {mismatched[0] // 2}" if mismatched else ""
    worst = max((abs(expected[i] - actual[i]) for i in mismatched), default=0)
    sys.exit(f"FAIL: {len(mismatched)} samples differ{first} (largest difference {worst}), {missing // 2} frames missing")

log = open(log_path, errors="replace").read()
for pattern in ("New latency[^\n]*", "frame adjustment[^\n]*"):
    found = re.findall(pattern, log)
    if found:
        print(f"shairport-sync: {found[-1].strip()}")

planned = re.search(r'pffr: "\d+/(\d+)"', log)
play_log = re.sub(r"\x1b\[[0-9;]*m", "", open(play_log_path, errors="replace").read())
sent = re.search(r"mach_ns=(\d+)", play_log)
if not planned or not sent:
    sys.exit("FAIL: no first-frame play time from shairport-sync or the stream")
error_us = (int(planned.group(1)) - int(sent.group(1))) / 1000
print(f"timing: shairport-sync plans the first frame {error_us:+.1f} us from the stream's play time")
if abs(error_us) > 1000:
    sys.exit("FAIL: more than 1 ms off")
EOF
