# Spaghetti

macOS audio routing tool. Routes a source (a tapped app, or a stream received over SRT or WebRTC) to destinations (any output device, an SRT peer, or a WebRTC peer). Ships as a virtual Core Audio driver, a daemon, and a CLI.

## Crates

- **`crates/asp-driver/`** — user-space Core Audio HAL plugin. Registers a virtual output device (`SpaghettiDevice_UID`) the system can target. Info.plist configures the bundle.
- **`crates/engine/`** (`spaghetti-engine`) — the audio engine as a library: the routing graph (`Router`), app and device listing, the virtual device registry (`DeviceManager`) and the driver's activity reader. A program can embed it instead of talking to the daemon.
- **`crates/daemon/`** (`spaghettid`) — long-running process. Owns an engine, source and route lifecycles, and an IPC control socket. Managed as a macOS LaunchAgent (`com.spaghetti.daemon`).
- **`crates/cli/`** (`spaghetti`) — user-facing CLI. `spaghetti tap <app>`, `spaghetti receive srt://...`, `spaghetti route <source_id> <device_uid|srt://...>`, `spaghetti daemon install/restart/logs`, etc. WebRTC peers come only from a signaling controller over IPC, such as `weave-adapter-spaghetti`; the CLI lists them but cannot create them.
- **`crates/common/`** — IPC protocol definitions (`ipc_protocol.rs`) and shared types.

## Boundaries

Spaghetti stands on its own. Add a capability only when it is general: useful to anyone using the driver, daemon, CLI or engine, and designed the way it would be without any particular consumer. Behavior that serves one program built on spaghetti belongs in that program. Do not shape APIs, defaults, the IPC protocol or data formats around one consumer, and do not reference closed-source consumers anywhere in this repository.

## Build + install

Daemon is installed system-wide. Standard loop after editing `crates/engine/` or `crates/daemon/`:

```bash
sudo chown -R $USER target/release
cargo build --release --workspace
codesign --sign - --force target/release/spaghettid
sudo cp target/release/spaghettid /usr/local/bin/spaghettid
spaghetti daemon restart
```

Daemon logs: `~/Library/Logs/spaghetti/spaghettid.log`. Follow with `spaghetti daemon logs --follow` or `tail -f`.

## Architecture: source → route pipeline

The routing path is the trickiest part. A tap routed to a device without a delay goes through that device's bus:

```
target apps
  → one process tap per route, a slot of the device's `DeviceBus`
  → one private aggregate device per destination device: the device as main sub-device
    (clock and output) plus every slot tap, drift-compensated by the HAL
  → one IOProc per bus reads each slot's tap and adds it, ramped to the route's volume,
    into the device's output buffer in the same cycle: no ring, resampler or jitter buffer
  → destination device
```

Changing an aggregate's tap list while it runs drops 20–35 ms of audio for every tap in it, but pointing a tap at other processes does not, so a bus keeps a fixed set of slot taps, points free ones at no process, and doubles them in one rebuild when full. Every tap costs `coreaudiod` CPU each cycle, empty or not. A started aggregate waits for a tapped app to play before it runs but keeps running after the apps stop, so the bus watcher restarts it once no slot's process has played for 3 s.

Every other route goes through a ring. Flow for a tap routed to SRT, WebRTC or a device on a delay:

```
target app
  → Core Audio process tap (AudioHardwareCreateProcessTap), opened while a ring route needs it
  → private aggregate device (wraps the tap + the default output as main sub-device for clocking)
  → one IOProc per tap (AudioDeviceCreateIOProcID) delivers AudioBufferList directly
  → per route sink (lock-free `RtSlots` the IOProc iterates) → SPSC ring of tap-rate frames
  → the route's output, as below
```

A network source routed to a device plays through an AUHAL:

```
SRT source: srt-tokio receive → TsDemuxer → ADTS → AudioToolbox AAC decode at 48 kHz
  → per route ring → per route output AUHAL render callback: rubato async resampler
    (source rate → dest rate) pulls exactly the input it needs; the AUHAL runs at the
    device-native format. A jitter buffer holds output silent until the ring reaches its
    target fill, and a PI controller on the fill trims the ratio by up to 0.5% so the
    source's clock and the destination's clock (mach time for virtual devices) stay matched

SRT destination: the route's ring → encoder thread (every 5 ms, 1024-frame chunks)
  → AudioToolbox AAC-LC 256 kb/s → ADTS → TsMuxer → 1316-byte messages → srt-tokio send

WebRTC source: str0m MediaData (40 ms reorder timeout) → decoder thread: one AudioToolbox
  Opus decode per packet, an empty-packet concealment per missing 20 ms of RTP time
  → 48 kHz stereo into the same per route rings, dated by the sender's clock once known

WebRTC destination: the route's ring → encoder thread (every 5 ms, 960-frame chunks)
  → AudioToolbox Opus 48 kHz stereo, 20 ms packets, RTP time +960 each, wallclock = capture time
  → one str0m Writer::write per packet, dropped until connected

WebRTC clock: the offerer adds an unordered, unretransmitted "clock" data channel. The
  receiving end probes every 100 ms, then every second after ten round trips; the sending end
  replies with its host clock and the RTP and capture time of its latest packet. `SenderClock`
  filters the round trips into the sender's offset and drift, and dates each decoded frame
  as captured on this host's clock, net of Opus's 312-frame pre-skip
```

Ring pushes carry the host time their first frame was captured, when the source knows it: a tap's IOProc input time, or a WebRTC source's date from its sender's clock. A device route with a delay plays on a schedule rather than on its fill: unprimed, `Playout` drops frames already too late and starts the earliest mid-render behind silence; primed, the PI controller trims the ratio against how far the tail frame runs from sounding `delay` after its capture, counting the device's own and its stream's latency. Routes with one delay play their source in step, within a frame or two on one host and within the clock estimate across hosts. SRT sources do not date their frames.

A source reports its `SourceTiming`: its rate, and its `DeliverySpan`, how far behind their capture its frames may land. A tap publishes its IOProc buffer; SRT and WebRTC sources measure the span from each push's media time and arrival, growing it at once past a stall it fell short of and shedding it slowly once delivery calms. `Playout` holds the live span on top of what the resampler needs, and re-primes once when the span grows under a playing route rather than resample audibly up to it.

Key files:
- `crates/engine/src/router.rs` — `Router`, the routing graph: sources, routes, the device buses, and attaching a route either to a bus or through a ring to its source and its output.
- `crates/engine/src/bus.rs` — `DeviceBus`: the slot taps and aggregate device that mix taps into one device, `bus_ioproc`, and the watcher that lets an idle bus wait for an app to play.
- `crates/engine/src/tap.rs` — `TapSource`: a process tap, its private aggregate device and the capture IOProc (`capture_ioproc`) that pushes into every attached route's ring, opened only while a ring route needs them. FFI wrappers around the ObjC bridge, and `list_audio_apps`.
- `crates/engine/src/device_output.rs` — `DeviceOutput`: a route's output AUHAL and `route_render_callback`.
- `crates/engine/src/playout.rs` — `Playout`, a route's output side: the resampler, jitter buffer and clock-drift controller between the ring and the render callback, or on a `Schedule`, the same controller holding each frame to its capture time plus the delay.
- `crates/engine/src/clock.rs` — `ClockFilter`, a two-state Kalman filter over a remote clock's offset and drift after Sendspin's time filter, the clock channel's `ClockMessage`s, and `SenderClock`, which dates a WebRTC source's frames.
- `crates/engine/src/ring.rs` — the SPSC ring of interleaved stereo frames, with the capture time of its latest push.
- `crates/engine/src/delivery.rs` — `DeliverySpan`, the span a source shares with its routes, and `DeliveryMeter`, which measures it for network sources.
- `crates/engine/src/sinks.rs` + `rt_slots.rs` — `Sinks`, the rings a source feeds, held in `RtSlots`, the lock-free handoff that lets routes join and leave a running capture.
- `crates/engine/bridge/tap_bridge.m` + `.h` — ObjC bridge for `AudioHardwareCreateProcessTap`, retargeting taps, and aggregate device creation. Compiled by `build.rs`.
- `crates/engine/src/srt.rs` — `SrtLink`, one SRT connection that reconnects when it drops, with its `FlowStatus`.
- `crates/engine/src/srt_receiver.rs` + `srt_sender.rs` — `SrtSource` (decode thread) and `SrtOutput` (encode thread).
- `crates/engine/src/webrtc.rs` — `PeerLink`, one str0m peer connection run by a tokio task on UDP sockets of its own, one per IPv4 interface that is up and each a host candidate, the default route's first (another interface's when there is none): STUN and TURN allocation on that first socket before the SDP (one 2 s bound), the offer or answer exchanged whole through `SetRemoteDescription` and `local_description`, transmits from a relayed address sent through TURN, its status (including whether recent datagrams came through a relay), and close on drop.
- `crates/engine/src/turn.rs` — `Relays`, a link's TURN allocations without I/O of their own: Allocate with the long-term credential (401 challenge, 438 stale nonce), Refresh at half the granted lifetime, CreatePermission for the remote's candidates, Send and Data indications, and release on close.
- `crates/engine/src/webrtc_receiver.rs` + `webrtc_sender.rs` — `WebRtcSource` (decode thread, gap concealment) and `WebRtcOutput` (encode thread).
- `crates/engine/src/stun.rs` — STUN messages: building and parsing them, XOR addresses, and long-term MESSAGE-INTEGRITY.
- `crates/engine/src/codec.rs` — `Encoder` and `Decoder` over AudioToolbox's `AudioConverter` for each `Codec` (AAC-LC, Opus). Every stream decodes to 48 kHz.
- `crates/engine/src/adts.rs` + `ts.rs` — ADTS framing of AAC access units, and a single-stream MPEG-TS muxer and demuxer.
- `crates/engine/src/flow.rs` — `FlowMeter`, which every source and output uses to report idle, connecting, connected, flowing or stalled.

## Core Audio gotchas (learned the hard way)

**Don't use AUHAL for tap capture.** AUHAL input units on a tap aggregate device return `-10863 kAudioUnitErr_CannotDoInCurrentContext` when you try to `AudioUnitRender`. Use `AudioDeviceCreateIOProcID` directly.

**Private tap aggregates need a main sub-device.** Without `kAudioAggregateDeviceMainSubDeviceKey` pointing at a real output device (currently we use the system default output), the aggregate has no clock source and the IOProc never fires. See `tap_bridge.m` `spaghetti_create_aggregate_for_tap`.

**`coreaudio_sys::AudioBufferList` is a flexible-array-member trap.** It's declared `mBuffers: [AudioBuffer; 1]`. Writing to `mBuffers.as_mut_ptr().add(i)` for `i ≥ 1` is out-of-provenance in Rust → UB → Core Audio's validator rejects the ABL with `-50 paramErr`. When building a multi-buffer ABL on the stack, use a `#[repr(C)]` wrapper struct with a correctly-sized array. When reading/writing a multi-buffer ABL handed in from C, compute byte offsets via `mem::offset_of!(AudioBufferList, mBuffers)` instead of going through the `[AudioBuffer; 1]` field.

**Tap ASBD ≠ IOProc delivery shape.** Process taps report themselves as one thing via `kAudioDevicePropertyStreamFormat` but can deliver a different layout in the IOProc. In practice we see **interleaved stereo f32** (`mNumberBuffers=1, mNumberChannels=2, mBytesPerFrame=8, mFormatFlags=9`) even when other codepaths imply non-interleaved. `push_buffers` in `tap.rs` accepts either layout.

**`AudioConverterFillComplexBuffer` output ABL must match the destination ASBD's layout.** For non-interleaved stereo output, the output ABL needs `mNumberBuffers = 2`, each with `mNumberChannels = 1`. Sizing it from the input ABL (which might be interleaved, so `mNumberBuffers = 1`) gives `-50 paramErr`.

**srt-tokio releases received packets unevenly.** Packets stamped 21.3 ms apart arrive 9 to 34 ms apart, so 25 ms of lateness is the floor of `SrtSource`'s measured delivery span. The sender stamps each packet with the capture time of its audio, from the ring's push times, rather than its send time; stamping with send time added the encoder's polling and AAC batching on top.

**The driver's activity region is system-wide.** Creating it unlinks the existing `/spaghetti.activity` first, so any process that loads the driver and creates the region replaces the live driver's, and every daemon started afterwards reads zeros until coreaudiod restarts. `Initialize` creates it only when given a host, which only coreaudiod passes; the driver tests pass none.

**Sample rate conversion happens per route on the output side, with rubato.** The render callback needs an exact frame count, which a fixed-output resampler provides by pulling a variable amount from the ring. `AudioConverter` was dropped because it resets its filter, and glitches, whenever its rate is changed at runtime.

## IPC

Unix domain socket at `spaghetti_common::ipc_protocol::SOCKET_PATH`. Request/response enum in `crates/common/src/ipc_protocol.rs`; bump `IPC_VERSION` when a payload changes shape. Daemon dispatches to the engine in `crates/daemon/src/server.rs`. The CLI client is `crates/cli/src/client.rs`.

## Virtual devices — the multi-device model

The plugin publishes N independent virtual devices, each with its own ring buffer, ASBD, and volume/mute controls. Per-device state lives in `DeviceInstance`; the plugin holds an `ArcSwap<DeviceRegistry>` keyed by object ID so IO callbacks resolve lock-free. The daemon is the sole writer of `/Library/Application Support/Spaghetti/devices.json` (fallback `/tmp/spaghetti.devices.json` when Library isn't writable), and the plugin polls the file's mtime every 500 ms and republishes `DeviceList` on change. The built-in `SpaghettiDevice_UID` always exists — destroying it is refused by the daemon, and the plugin re-inserts it even if the config omits it. Object IDs are monotonically allocated in blocks of 5 (device + 2 streams + volume + mute) and never recycle.

```json
{
  "version": 1,
  "devices": [
    { "uid": "SpaghettiDevice_UID",            "name": "Spaghetti",    "channels": 2, "sample_rate": 48000 },
    { "uid": "spaghetti_virtual_<uuid>",       "name": "Stream Audio", "channels": 2, "sample_rate": 48000 }
  ]
}
```

The activity shm is still single-region — only the first (default) device feeds frame counters and peak into the shared region. Multi-device activity reporting is follow-up work.

## Conventions

- Edition 2024.
- `anyhow` for app-level errors, `Result<T, String>` in some places (daemon_mgmt, router setup).
- Logging via `tracing`; default filter is `info`. Real-time paths (IOProc, render callback) use first-fire gates so we log once per route, not per callback.
- No heap allocation in real-time callbacks. Pre-allocate buffers in `Playout` / `OutputCallbackState`.
- No AI attribution in commits.

## Diagnostic logging

Real-time paths emit first-fire diagnostics that are invaluable for debugging format/rate mismatches:
- `tap ASBD` — source format from `query_device_stream_format`.
- `destination device format` — the device side of the output AUHAL; its rate is the route's rate.
- `dst_asbd` — the output AUHAL's client format.
- `created resampler` — tap and device rates and the resampler's delay in frames.
- `IOProc first call` — the ABL shape the tap actually delivers.
- `render callback first fire` — output ABL layout and the ring fill at the first render.
- `route playout health` (debug, every 10 s) — each route's smoothed ring fill against its target, its source's delivery span (`delivery_ms`), the controller's ratio correction in ppm, and glitch counters.
- `route playout glitched since the last report` (info) — underruns, re-primes to a grown span, resyncs and ring overflows since the previous report, and the span and target at the latest of them (`glitch_delivery_ms`, `glitch_target_ms`).
- `SRT connected` / `SRT sender disconnected` / `SRT receiver disconnected` (info) — each SRT link's peer coming and going.
- `SRT stream format` (info) — the AAC rate and channels of a received stream, logged when they change.
- `route SRT health` (debug, every 10 s) — an SRT route's state, bytes and packet counters.

Grep these strings in `~/Library/Logs/spaghetti/spaghettid.log` when routing breaks.
