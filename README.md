# Spaghetti

Route macOS app audio to physical or virtual output devices, to AirPlay receivers, or to other machines over SRT, from the CLI. [weave-adapter-spaghetti](https://github.com/kristalalfred/weave-adapter-spaghetti) adds WebRTC links signaled by open-weave.

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

Use an output UID from `system-devices` and a route ID from `routes`. `system-devices` lists each device's input and output channels, and needs no daemon.

Play a source in step on several outputs by giving each route the same delay in milliseconds. Each frame then sounds that long after it was captured, including on Macs that receive the source over WebRTC. Taps and WebRTC sources can play on a delay; SRT sources cannot:

```sh
spaghetti route 1 BuiltInSpeakerDevice --delay 300
spaghetti route 1 '<device-uid>' --delay 300
```

Routes play in step as far as each output reports its own latency. `--latency-offset` corrects an output that sounds later or earlier than it reports, in milliseconds: positive when it sounds later, negative when earlier. It applies only to a route with a delay:

```sh
spaghetti route 1 '<device-uid>' --delay 300 --latency-offset 20
```

`calibrate` measures the offset with a microphone. It plays probe trains through the reference and the destination in turn, each on its own delay, until two of the destination's trains about 10 s apart agree against the reference within 0.25 ms and none drifts more than 0.02 ms a second: about half a minute for a destination that settles at once, and a failure saying its timing kept moving after five of its trains, about a minute, for one that does not. It prints how late each was heard against its schedule and the `--latency-offset` that makes the destination sound with the reference where the microphone is. A successful calibration can measure a reflection instead of the direct sound when the direct path is more than 21 dB weaker. The microphone's own timing cancels out; moving the microphone changes the result. It runs in the CLI process, with the default input unless `--microphone` names an input UID, and plays audible chirps. `--latency-offset` and `--reference-latency-offset` measure with offsets already applied; calibrating again with the printed offset prints it again. Ctrl-C stops it:

```sh
spaghetti calibrate 'airplay://Kitchen' --reference BuiltInSpeakerDevice
spaghetti route 1 'airplay://Kitchen' --delay 3000 --latency-offset 212
```

A destination and its reference play on `--delay`, 3000 ms unless given, and `--reference-delay`, the same unless given. A receiver a daemon route plays to is refused.

Play a source on an AirPlay receiver. An AirPlay route always plays on a delay of at least about 2.1 s; the error for a shorter delay gives the minimum for the source. Give a local route the same delay to play in step:

```sh
spaghetti airplay
spaghetti route 1 'airplay://Living Room' --delay 2500
spaghetti route 1 BuiltInSpeakerDevice --delay 2500
```

`airplay` lists the receivers on the local network and whether spaghetti can play to each. Spaghetti sends uncompressed ALAC over AirPlay 2 to receivers that pair without a PIN, and over AirPlay 1 (RAOP) to the rest. A receiver that requires a password or a PIN is listed with the reason it is unsupported. `airplay://` takes a receiver's name or its ID. A receiver that reports no latency of its own is assumed to add 250 ms, the AirPlay default; one that adds a different amount plays early or late by the difference, which `--latency-offset` corrects. A receiver takes one route at a time. The route's volume scales the audio; the receiver's own volume is left as it is. SRT sources cannot play to AirPlay receivers.

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

The `spaghetti-engine` crate runs the routing graph in a program's own process, without the daemon. Besides taps, devices, AirPlay, SRT and WebRTC, a program can register its own audio I/O: named inputs it captures and outputs it plays from its own real-time callbacks. Sources and routes then name them like any other endpoint, and come and go while the callbacks keep running.

```rust
use std::time::Duration;

use spaghetti_engine::{Destination, Frames, HostTime, Router, SourceSpec};

fn example(captured: &[f32], left: &mut [f32], right: &mut [f32]) -> Result<(), Box<dyn std::error::Error>> {
    let mut router = Router::new();
    let mut microphone = router.add_program_input("microphone", 48_000.0)?;
    let mut speaker = router.add_program_output("speaker", 48_000.0, Duration::from_millis(10))?;

    let source = router.create_source(SourceSpec::Program { input: "microphone".into() }, None)?;
    let speaker_route = Destination::Program {
        output: "speaker".into(),
        delay_ms: Some(300),
        latency_offset_ms: None,
    };
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
| Rates | Each input and output has its own rate, at most `MAX_PROGRAM_RATE` (768 kHz). Every route resamples from its source's rate to its output's rate. |
| Delay | With `delay_ms`, a route plays each frame that long after it was captured, as on a device. `latency_offset_ms` corrects an output heard later (positive) or earlier (negative) than the `heard` it renders by, and requires a delay. A delay is refused when the route would have to hold more audio than its buffer can hold beside the resampler's maximum input and 20 ms of scheduling slack: the delay less the output's declared latency and the latency offset. A route from any source holds at least the delay a 48 kHz source's route holds at the same output rate, 5353 ms at a 48 kHz output: its buffer is 262144 frames for a 48 kHz source, and at other rates the fewest frames in a power of two that hold as long beside the resampler's input and the slack. SRT sources do not date their frames and cannot play on a delay. |
| Discontinuities | A push dated more than 1 ms off where the previous one ended, a push a route had no room for, and a push after `ProgramInput::discontinuity` each start a new run. A route drops what it still holds from before the new run and primes again. |
| Several routes | One output mixes up to `MAX_PROGRAM_ROUTES` (8) routes. One input feeds up to 8 routes. |
| Removal | `remove_program_input` destroys the sources made from the input. `remove_program_output` fades out and destroys the routes to the output. The real-time ends keep working: an input feeds nothing, an output renders silence. Drop them outside the real-time thread. |
| Input devices | macOS only. `SourceSpec::Input { device_uid }` captures a Core Audio input device's first input stream, its first two channels or its one channel on both, while a route or calibration takes its frames. Each frame is dated by the device's time stamp less its input latency, and a change to the device's format, rate, latency or streams, or its removal, ends the source as `Lost`, with the reason in `SourceInfo::failure`. |
| Calibration | `Router::calibrate` measures how much later one destination on a delay sounds than another where a recording hears both, a `Recording::Input` device or a `Recording::Program` input that dates its pushes, and returns at once. `Calibrating::state` reports progress and the result: both residuals and the output's `latency_offset_ms` that aligns it with the reference. It waits, within a bound, for the output's timing to settle, and fails with `Unsettled` when it does not. The calibration holds its AirPlay receivers and program outputs against new routes while it runs, ends with `EndpointRemoved` when a program input or output it uses is removed or the router drops, and releases everything it opened before reporting `Done`. `Calibrating::cancel`, or dropping the handle, stops it. A program input recording must date each push within a quarter frame of the line through its pushes. |
| State | `Router::list_routes` reports each device or program route's `Playback`: `Playing` when the output's latest render played the route's source at any volume, `Silent` while the route waits for audio or for its schedule or after a refused render, and `Stopped` when the output has not rendered for 100 ms plus 4096 frames at its rate. `list_sources` reports a program source as `Idle` without routes, then `Connected`, `Flowing` while the input pushes, and `Stalled` 3 s after it stops. |

The engine also builds for iOS (`aarch64-apple-ios`, `aarch64-apple-ios-sim`). Those targets have no Core Audio HAL: process taps, input and output devices and the virtual-device functions are unavailable, and a `SourceSpec::Tap`, `SourceSpec::Input` or `Destination::Device` is refused when it is created. Program I/O, SRT, WebRTC and AirPlay remain available.

```sh
just check-ios
```
