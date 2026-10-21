# Spaghetti

macOS audio routing tool. Lets you tap audio from a running app (by PID) and route it to any output device. Ships as a virtual Core Audio driver, a daemon, a CLI, and a TUI.

## Crates

- **`crates/asp-driver/`** — user-space Core Audio HAL plugin. Registers a virtual output device (`SpaghettiDevice_UID`) the system can target. Info.plist configures the bundle.
- **`crates/daemon/`** (`spaghettid`) — long-running process. Owns the audio routing graph, the tap lifecycle, and an IPC control socket. Managed as a macOS LaunchAgent (`com.spaghetti.daemon`).
- **`crates/cli/`** (`spaghetti`) — user-facing CLI. `spaghetti tap <pid>`, `spaghetti route <tap_id> <device_uid>`, `spaghetti daemon install/restart/logs`, etc.
- **`crates/tui/`** (`spaghetti-tui`) — ratatui-based terminal UI. Polls the daemon and sends IPC requests. Uses `tokio::sync::RwLock` for shared state.
- **`crates/common/`** — IPC protocol definitions (`ipc_protocol.rs`), async client (`async_client.rs`), shared types.

## Build + install

Daemon is installed system-wide. Standard loop after editing `crates/daemon/`:

```bash
sudo chown -R $USER target/release
cargo build --release --workspace
sudo cp target/release/spaghettid /usr/local/bin/spaghettid
spaghetti daemon restart
```

For TUI iteration just `cargo run -p spaghetti-tui` (or rebuild and run the installed binary).

Daemon logs: `~/Library/Logs/spaghetti/spaghettid.log`. Follow with `spaghetti daemon logs --follow` or `tail -f`.

## Architecture: tap → route pipeline

The routing path is the trickiest part. Flow:

```
target app
  → Core Audio process tap (AudioHardwareCreateProcessTap)
  → private aggregate device (wraps the tap + a main sub-device for clocking)
  → IOProc (AudioDeviceCreateIOProcID) delivers AudioBufferList directly
  → AudioConverter (SRC: tap rate → dest rate, interleaved → non-interleaved)
  → SPSC ring buffer (in router.rs)
  → output AUHAL render callback (runs at device-native format, no conversion)
  → destination device
```

Key files:
- `crates/daemon/src/router.rs` — the whole routing graph. `setup_audio_route`, `route_ioproc`, `route_render_callback`, ring buffer.
- `crates/daemon/bridge/tap_bridge.m` + `.h` — ObjC bridge for `AudioHardwareCreateProcessTap` and aggregate device creation. Compiled by `build.rs`.
- `crates/daemon/src/tap.rs` — `TapManager`, FFI wrappers around the ObjC bridge.

## Core Audio gotchas (learned the hard way)

**Don't use AUHAL for tap capture.** AUHAL input units on a tap aggregate device return `-10863 kAudioUnitErr_CannotDoInCurrentContext` when you try to `AudioUnitRender`. Use `AudioDeviceCreateIOProcID` directly — this is the pattern Apple's `AudioCap` sample uses.

**Private tap aggregates need a main sub-device.** Without `kAudioAggregateDeviceMainSubDeviceKey` pointing at a real output device (currently we use the system default output), the aggregate has no clock source and the IOProc never fires. See `tap_bridge.m` `spaghetti_create_aggregate_for_tap`.

**`coreaudio_sys::AudioBufferList` is a flexible-array-member trap.** It's declared `mBuffers: [AudioBuffer; 1]`. Writing to `mBuffers.as_mut_ptr().add(i)` for `i ≥ 1` is out-of-provenance in Rust → UB → Core Audio's validator rejects the ABL with `-50 paramErr`. When building a multi-buffer ABL on the stack, use a `#[repr(C)]` wrapper struct with a correctly-sized array (see `Abl2` in `router.rs`). When reading/writing a multi-buffer ABL handed in from C, compute byte offsets via `mem::offset_of!(AudioBufferList, mBuffers)` instead of going through the `[AudioBuffer; 1]` field.

**Tap ASBD ≠ IOProc delivery shape.** Process taps report themselves as one thing via `kAudioDevicePropertyStreamFormat` but can deliver a different layout in the IOProc. In practice we see **interleaved stereo f32** (`mNumberBuffers=1, mNumberChannels=2, mBytesPerFrame=8, mFormatFlags=9`) even when other codepaths imply non-interleaved. Design the converter + output ABL shape around `dst_asbd`, not around the input ABL's buffer count.

**`AudioConverterFillComplexBuffer` output ABL must match the destination ASBD's layout.** For non-interleaved stereo output, the output ABL needs `mNumberBuffers = 2`, each with `mNumberChannels = 1`. Sizing it from the input ABL (which might be interleaved, so `mNumberBuffers = 1`) gives `-50 paramErr`.

**Sample rate conversion: do it on the input side.** Converting in the IOProc before the ring buffer means ring + output AUHAL both run at destination-native rate. Cleaner than plumbing an `AudioConverter` through the render callback.

## IPC

Unix domain socket at `spaghetti_common::ipc_protocol::SOCKET_PATH`. Request/response enum in `crates/common/src/ipc_protocol.rs`. Daemon dispatches in `crates/daemon/src/server.rs`. Clients: sync `crates/cli/src/client.rs`, async `crates/common/src/async_client.rs` (used by TUI).

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

## TUI state

`crates/tui/src/main.rs` owns a `tokio::sync::RwLock<State>`. Render loop reads, input/poller tasks write.

**Don't hold a read guard across a write-lock await on the same lock.** Rust variable shadowing does not drop the original binding — the outer read guard stays alive until its block ends. `tokio::RwLock` is write-preferring, so a task holding a read and awaiting a write deadlocks against queued writers. Pattern: scope the read to its own `{ ... }` block, then acquire the write.

## Conventions

- Edition 2024.
- `anyhow` for app-level errors, `Result<T, String>` in some places (daemon_mgmt, router setup).
- Logging via `tracing`; default filter is `info`. Real-time paths (IOProc, render callback) use first-fire `AtomicBool` gates so we log once per route, not per callback.
- No heap allocation in real-time callbacks. Pre-allocate buffers in `InputCallbackState` / `OutputCallbackState`.
- No AI attribution in commits.

## Diagnostic logging in router.rs

Real-time paths emit first-fire diagnostics that are invaluable for debugging format/rate mismatches:
- `tap ASBD` — source format from `query_device_stream_format`.
- `output unit native format` — destination format before any override.
- `dst_asbd` — the converter output format we synthesize.
- `IOProc first call — pre-converter parameters` — input/output ABL shapes fed to `AudioConverterFillComplexBuffer`.
- `IOProc first call — post-converter result` — `conv_status` and actual frame count.
- `render callback first fire` — output ABL layout and `frames_popped` from ring.

Grep these strings in `~/Library/Logs/spaghetti/spaghettid.log` when routing breaks.
