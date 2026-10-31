# Spaghetti

Route macOS app audio to physical or virtual output devices, or to other machines over SRT, from the CLI. [weave-adapter-spaghetti](https://github.com/kristalalfred/weave-adapter-spaghetti) adds WebRTC links signaled by open-weave.

Requires macOS, Rust, and Xcode Command Line Tools.

```sh
./scripts/install.sh
```

Find an app, tap its audio as a source, and route it to the default Spaghetti device:

```sh
spaghetti apps
spaghetti tap Safari
spaghetti route 1
```

Use the source ID printed by `tap`. Add `--mute` to `tap` to silence the app's original output.

Choose an output and adjust a route:

```sh
spaghetti system-devices
spaghetti route 1 '<device-uid>' --volume 0.5
spaghetti routes
spaghetti volume 1 0.8
```

Use an output UID from `system-devices` and a route ID from `routes`.

Play a source in step on several outputs by giving each route the same delay in milliseconds. Each frame then sounds that long after it was captured, including on Macs that receive the source over WebRTC. Taps and WebRTC sources can play on a delay; SRT sources cannot:

```sh
spaghetti route 1 BuiltInSpeakerDevice --delay 300
spaghetti route 1 '<device-uid>' --delay 300
```

Send a source to another machine over SRT, and play what it receives. `srt://:PORT` listens, `srt://HOST:PORT` calls; one side of a link listens and the other calls:

```sh
spaghetti route 1 srt://:9000 --passphrase 'shared secret 1'
spaghetti receive srt://sending-mac.local:9000 --passphrase 'shared secret 1'
spaghetti route 2 BuiltInSpeakerDevice
```

Audio travels as AAC-LC in MPEG-TS. `--latency` sets the SRT latency in milliseconds, 200 by default. `SPAGHETTI_SRT_PASSPHRASE` can replace `--passphrase`, which keeps the passphrase out of the process list.

List sources, and remove a route or a source with its routes:

```sh
spaghetti sources
spaghetti unroute 1
spaghetti unsource 1
```

CLI help: `spaghetti --help`. [MIT license](LICENSE).

## Embedding the engine

The `spaghetti-engine` crate runs the routing graph in a program's own process, without the daemon. Besides taps, devices, SRT and WebRTC, a program can register its own audio I/O: named inputs it captures and outputs it plays from its own real-time callbacks. Sources and routes then name them like any other endpoint, and come and go while the callbacks keep running.

```rust
use std::time::Duration;

use spaghetti_engine::{Destination, Frames, HostTime, Router, SourceSpec};

fn example(captured: &[f32], left: &mut [f32], right: &mut [f32]) -> Result<(), Box<dyn std::error::Error>> {
    let mut router = Router::new();
    let mut microphone = router.add_program_input("microphone", 48_000.0)?;
    let mut speaker = router.add_program_output("speaker", 48_000.0, Duration::from_millis(10))?;

    let source = router.create_source(SourceSpec::Program { input: "microphone".into() }, None)?;
    let speaker_route = Destination::Program { output: "speaker".into(), delay_ms: Some(300) };
    router.create_route(source, speaker_route, 1.0)?;

    // In the capture callback, dated by when the buffer's first frame was captured:
    microphone.push(Frames::mono(captured)?, HostTime::now());
    // In the playback callback, dated by when its first frame will be heard:
    speaker.render([left, right], HostTime::now() + Duration::from_millis(10))?;
    Ok(())
}
```

| Item | Contract |
| --- | --- |
| Clock | `HostTime` is `mach_absolute_time`, the clock of `AudioTimeStamp::mHostTime`. |
| `ProgramInput::push` | Real-time safe. 32-bit float frames, mono (played on both channels at its own level), interleaved stereo or planar stereo, at the input's rate. `captured` is the host time the first frame was captured. Pushes of unbroken audio are dated their length apart. |
| `ProgramOutput::render` | Real-time safe. Fills two planes at the output's rate with every route to the output, mixed and clamped to [-1, 1]. `heard` is the host time the first frame is heard, output latency included. |
| Buffer size | At most `MAX_CALLBACK_FRAMES` (4096) frames per push or render. `Frames` refuses longer buffers, half-filled interleaved frames and planes of unequal length with a `BufferError`. `render` refuses unequal or longer planes and fills them with silence. |
| Rates | Each input and output has its own rate. Every route resamples from its source's rate to its output's rate. |
| Delay | With `delay_ms`, a route plays each frame that long after it was captured, as on a device. A delay is refused when the route would have to hold more audio than its 131072-frame buffer: the delay less the output's declared latency, at the source's rate. SRT sources do not date their frames and cannot play on a delay. |
| Discontinuities | A push dated more than 1 ms off where the previous one ended, a push a route had no room for, and a push after `ProgramInput::discontinuity` each start a new run. A route drops what it still holds from before the new run and primes again. |
| Several routes | One output mixes up to `MAX_PROGRAM_ROUTES` (8) routes. One input feeds up to 8 routes. |
| Removal | `remove_program_input` destroys the sources made from the input. `remove_program_output` fades out and destroys the routes to the output. The real-time ends keep working: an input feeds nothing, an output renders silence. Drop them outside the real-time thread. |
| State | `Router::list_routes` reports each device or program route's `Playback`: `Playing` when the output's latest render played the route's source at any volume, `Silent` while the route waits for audio or for its schedule or after a refused render, and `Stopped` when the output has not rendered for 100 ms plus 4096 frames at its rate. `list_sources` reports a program source as `Idle` without routes, then `Connected`, `Flowing` while the input pushes, and `Stalled` 3 s after it stops. |

The engine also builds for iOS (`aarch64-apple-ios`, `aarch64-apple-ios-sim`). Those targets have no Core Audio HAL: process taps, device outputs and the virtual-device functions are unavailable, and a `SourceSpec::Tap` or `Destination::Device` is refused when it is created. Program I/O, SRT and WebRTC work as on macOS.

```sh
just check-ios
```
