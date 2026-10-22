# Spaghetti

macOS audio routing tool. Routes a source (a tapped app, or a stream received over SRT) to destinations (any output device, or an SRT peer). Ships as a virtual Core Audio driver, a daemon, and a CLI.

## Crates

- **`crates/asp-driver/`** — user-space Core Audio HAL plugin. Registers a virtual output device (`SpaghettiDevice_UID`) the system can target. Info.plist configures the bundle.
- **`crates/daemon/`** (`spaghettid`) — long-running process. Owns the audio routing graph, source and route lifecycles, and an IPC control socket. Managed as a macOS LaunchAgent (`com.spaghetti.daemon`).
- **`crates/cli/`** (`spaghetti`) — user-facing CLI. `spaghetti tap <app>`, `spaghetti receive srt://...`, `spaghetti route <source_id> <device_uid|srt://...>`, `spaghetti daemon install/restart/logs`, etc.
- **`crates/common/`** — IPC protocol definitions (`ipc_protocol.rs`) and shared types.

## Build + install

Daemon is installed system-wide. Standard loop after editing `crates/daemon/`:

```bash
sudo chown -R $USER target/release
cargo build --release --workspace
sudo cp target/release/spaghettid /usr/local/bin/spaghettid
spaghetti daemon restart
```

Daemon logs: `~/Library/Logs/spaghetti/spaghettid.log`. Follow with `spaghetti daemon logs --follow` or `tail -f`.

## Architecture: source → route pipeline

The routing path is the trickiest part. Flow for a tap routed to a device:

```
target app
  → Core Audio process tap (AudioHardwareCreateProcessTap)
  → private aggregate device (wraps the tap + a main sub-device for clocking)
  → one IOProc per tap (AudioDeviceCreateIOProcID) delivers AudioBufferList directly
  → per route sink (lock-free `RtSlots` the IOProc iterates) → SPSC ring of tap-rate frames
  → per route output AUHAL render callback: rubato async resampler (tap rate → dest rate)
    pulls exactly the input it needs; the AUHAL runs at the device-native format.
    A jitter buffer holds output silent until the ring reaches its target fill, and a PI
    controller on the fill trims the ratio by up to 0.5% so the tap's clock (the default
    output's hardware) and the destination's clock (mach time for virtual devices) stay matched
  → destination device
```

SRT ends replace either side:

```
SRT source: srt-tokio receive → TsDemuxer → ADTS → AudioToolbox AAC decode at 48 kHz
  → the same per route rings, Playout and AUHAL as a tap

SRT destination: the route's ring → encoder thread (every 5 ms, 1024-frame chunks)
  → AudioToolbox AAC-LC 256 kb/s → ADTS → TsMuxer → 1316-byte messages → srt-tokio send
```

A source reports its `SourceTiming`: its rate, and how unevenly it pushes. `Playout` sizes its jitter buffer from it.

Key files:
- `crates/daemon/src/router.rs` — `Router`, the routing graph: sources, routes, and attaching a route's ring to its source and its output.
- `crates/daemon/src/tap.rs` — `TapSource`: a process tap, its private aggregate device and the capture IOProc (`capture_ioproc`) that pushes into every attached route's ring. FFI wrappers around the ObjC bridge, and `list_audio_apps`.
- `crates/daemon/src/device_output.rs` — `DeviceOutput`: a route's output AUHAL and `route_render_callback`.
- `crates/daemon/src/playout.rs` — `Playout`, a route's output side: the resampler, jitter buffer and clock-drift controller between the ring and the render callback.
- `crates/daemon/src/ring.rs` — the SPSC ring of interleaved stereo frames.
- `crates/daemon/src/sinks.rs` + `rt_slots.rs` — `Sinks`, the rings a source feeds, held in `RtSlots`, the lock-free handoff that lets routes join and leave a running capture.
- `crates/daemon/bridge/tap_bridge.m` + `.h` — ObjC bridge for `AudioHardwareCreateProcessTap` and aggregate device creation. Compiled by `build.rs`.
- `crates/daemon/src/srt.rs` — `SrtLink`, one SRT connection that reconnects when it drops, with its `FlowStatus`.
- `crates/daemon/src/srt_receiver.rs` + `srt_sender.rs` — `SrtSource` (decode thread) and `SrtOutput` (encode thread).
- `crates/daemon/src/aac.rs` + `ts.rs` — the AudioToolbox AAC codec, ADTS framing, and a single-stream MPEG-TS muxer and demuxer.
- `crates/daemon/src/flow.rs` — `FlowMeter`, which every source and output uses to report idle, connecting, connected, flowing or stalled.

## Core Audio gotchas (learned the hard way)

**Don't use AUHAL for tap capture.** AUHAL input units on a tap aggregate device return `-10863 kAudioUnitErr_CannotDoInCurrentContext` when you try to `AudioUnitRender`. Use `AudioDeviceCreateIOProcID` directly.

**Private tap aggregates need a main sub-device.** Without `kAudioAggregateDeviceMainSubDeviceKey` pointing at a real output device (currently we use the system default output), the aggregate has no clock source and the IOProc never fires. See `tap_bridge.m` `spaghetti_create_aggregate_for_tap`.

**`coreaudio_sys::AudioBufferList` is a flexible-array-member trap.** It's declared `mBuffers: [AudioBuffer; 1]`. Writing to `mBuffers.as_mut_ptr().add(i)` for `i ≥ 1` is out-of-provenance in Rust → UB → Core Audio's validator rejects the ABL with `-50 paramErr`. When building a multi-buffer ABL on the stack, use a `#[repr(C)]` wrapper struct with a correctly-sized array. When reading/writing a multi-buffer ABL handed in from C, compute byte offsets via `mem::offset_of!(AudioBufferList, mBuffers)` instead of going through the `[AudioBuffer; 1]` field.

**Tap ASBD ≠ IOProc delivery shape.** Process taps report themselves as one thing via `kAudioDevicePropertyStreamFormat` but can deliver a different layout in the IOProc. In practice we see **interleaved stereo f32** (`mNumberBuffers=1, mNumberChannels=2, mBytesPerFrame=8, mFormatFlags=9`) even when other codepaths imply non-interleaved. `push_buffers` in `tap.rs` accepts either layout.

**`AudioConverterFillComplexBuffer` output ABL must match the destination ASBD's layout.** For non-interleaved stereo output, the output ABL needs `mNumberBuffers = 2`, each with `mNumberChannels = 1`. Sizing it from the input ABL (which might be interleaved, so `mNumberBuffers = 1`) gives `-50 paramErr`.

**srt-tokio releases received packets unevenly.** Packets stamped 21.3 ms apart arrive 9 to 34 ms apart, so `SrtSource` declares 25 ms of delivery jitter and routes from it buffer that much more. The sender stamps each packet with the capture time of its audio, from the ring's push times, rather than its send time; stamping with send time added the encoder's polling and AAC batching on top.

**The driver's activity region is system-wide.** Creating it unlinks the existing `/spaghetti.activity` first, so any process that loads the driver and creates the region replaces the live driver's, and every daemon started afterwards reads zeros until coreaudiod restarts. `Initialize` creates it only when given a host, which only coreaudiod passes; the driver tests pass none.

**Sample rate conversion happens per route on the output side, with rubato.** The render callback needs an exact frame count, which a fixed-output resampler provides by pulling a variable amount from the ring. `AudioConverter` was dropped because it resets its filter, and glitches, whenever its rate is changed at runtime.

## IPC

Unix domain socket at `spaghetti_common::ipc_protocol::SOCKET_PATH`. Request/response enum in `crates/common/src/ipc_protocol.rs`; bump `IPC_VERSION` when a payload changes shape. Daemon dispatches in `crates/daemon/src/server.rs`. The CLI client is `crates/cli/src/client.rs`.

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
- `route playout health` (debug, every 10 s) — each route's smoothed ring fill against its target, the controller's ratio correction in ppm, and glitch counters.
- `route playout glitched since the last report` (info) — underruns (re-primes), resyncs and ring overflows since the previous report.
- `SRT connected` / `SRT sender disconnected` / `SRT receiver disconnected` (info) — each SRT link's peer coming and going.
- `SRT stream format` (info) — the AAC rate and channels of a received stream, logged when they change.
- `route SRT health` (debug, every 10 s) — an SRT route's state, bytes and packet counters.

Grep these strings in `~/Library/Logs/spaghetti/spaghettid.log` when routing breaks.
