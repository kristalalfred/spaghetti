use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use airplay::{
    Browser, FRAMES_PER_PACKET, Protocol, ReceiverId, Render, SAMPLE_RATE, Status, Stream,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: play
       play RECEIVER (--sine HZ | --wav FILE) [--seconds N] [--gain DB] [--airplay 1|2]

Without arguments, lists the AirPlay receivers on the network. RECEIVER is a
receiver's name or device ID. --wav plays 16-bit 44.1 kHz stereo; --gain
defaults to -20 dB. --airplay speaks that protocol and no other; without it the
stream speaks AirPlay 2 to receivers that pair transiently and AirPlay 1 to the
rest.";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let browser = Browser::start();
    let Some(receiver) = args.first() else {
        return list(&browser);
    };
    let options = Options::parse(&args[1..])?;
    let id = match receiver.parse::<ReceiverId>() {
        Ok(id) => id,
        Err(_) => find_by_name(&browser, receiver)?,
    };
    play(&browser, id, options)
}

fn list(browser: &Browser) -> Result<(), String> {
    for receiver in browser.receivers()? {
        let support = match receiver.protocol() {
            Ok(protocol) => format!("plays over {protocol}"),
            Err(reason) => format!("unsupported: {reason}"),
        };
        let model = receiver.model.as_deref().unwrap_or("?");
        println!("{}  {}  ({model})  {support}", receiver.id, receiver.name);
    }
    Ok(())
}

fn find_by_name(browser: &Browser, name: &str) -> Result<ReceiverId, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(receiver) = browser.receivers()?.into_iter().find(|r| r.name == name) {
            return Ok(receiver.id);
        }
        if Instant::now() >= deadline {
            return Err(format!("no AirPlay receiver named {name:?} found"));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

struct Options {
    source: Source,
    seconds: Option<f64>,
    gain: f32,
    protocol: Option<Protocol>,
}

enum Source {
    Sine(f64),
    Wav(Vec<[f32; 2]>),
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut source = None;
        let mut seconds = None;
        let mut gain_db = -20.0f32;
        let mut protocol = None;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{flag} needs a value\n\n{USAGE}"))
            };
            let number = |value: &str| {
                value
                    .parse::<f64>()
                    .map_err(|_| format!("{flag} takes a number, not {value:?}"))
            };
            match flag.as_str() {
                "--sine" => source = Some(Source::Sine(number(value()?)?)),
                "--wav" => source = Some(Source::Wav(read_wav(value()?)?)),
                "--seconds" => seconds = Some(number(value()?)?),
                "--gain" => gain_db = number(value()?)? as f32,
                "--airplay" => {
                    protocol = Some(match value()?.as_str() {
                        "1" => Protocol::AirPlay1,
                        "2" => Protocol::AirPlay2,
                        other => return Err(format!("--airplay takes 1 or 2, not {other:?}")),
                    })
                }
                _ => return Err(format!("unknown option {flag:?}\n\n{USAGE}")),
            }
        }
        let source = source.ok_or_else(|| format!("choose --sine or --wav\n\n{USAGE}"))?;
        Ok(Options {
            source,
            seconds,
            gain: 10f32.powf(gain_db / 20.0),
            protocol,
        })
    }

    fn frames(&self) -> u64 {
        let requested = self.seconds.map(|s| (s * f64::from(SAMPLE_RATE)) as u64);
        match &self.source {
            Source::Sine(_) => requested.unwrap_or(5 * u64::from(SAMPLE_RATE)),
            Source::Wav(frames) => {
                requested.map_or(frames.len() as u64, |r| r.min(frames.len() as u64))
            }
        }
    }

    fn frame(&self, index: u64) -> [f32; 2] {
        let [left, right] = match &self.source {
            Source::Sine(hz) => {
                let t = index as f64 / f64::from(SAMPLE_RATE);
                let sample = (2.0 * std::f64::consts::PI * hz * t).sin() as f32;
                [sample, sample]
            }
            Source::Wav(frames) => frames[index as usize],
        };
        [left * self.gain, right * self.gain]
    }
}

fn read_wav(path: &str) -> Result<Vec<[f32; 2]>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let invalid = |why: &str| format!("{path}: {why}");
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid("not a WAV file"));
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut at = 12;
    let mut format_ok = false;
    while at + 8 <= bytes.len() {
        let (id, len) = (&bytes[at..at + 4], u32_at(at + 4) as usize);
        let body = at + 8;
        let end = (body + len).min(bytes.len());
        match id {
            b"fmt " if len >= 16 => {
                format_ok = u16_at(body) == 1
                    && u16_at(body + 2) == 2
                    && u32_at(body + 4) == SAMPLE_RATE
                    && u16_at(body + 14) == 16;
            }
            b"data" => {
                if !format_ok {
                    return Err(invalid("not 16-bit 44.1 kHz stereo PCM"));
                }
                let sample =
                    |at: usize| f32::from(i16::from_le_bytes([bytes[at], bytes[at + 1]])) / 32768.0;
                return Ok((body..end - 3)
                    .step_by(4)
                    .map(|at| [sample(at), sample(at + 2)])
                    .collect());
            }
            _ => {}
        }
        at = body + len + len % 2;
    }
    Err(invalid("no audio data"))
}

/// Claude: an `Instant` as nanoseconds of `mach_absolute_time`, the clock
/// receivers on this Mac such as shairport-sync log in.
fn mach_time(at: Instant) -> u64 {
    let now = Instant::now();
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) };
    let mach_now = time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64;
    let ahead = at.saturating_duration_since(now).as_nanos() as u64;
    let behind = now.saturating_duration_since(at).as_nanos() as u64;
    mach_now + ahead - behind
}

fn play(browser: &Browser, id: ReceiverId, options: Options) -> Result<(), String> {
    let protocol = options.protocol;
    let total = options.frames();
    let rendered_all = Arc::new(AtomicBool::new(false));
    let mut first_play_at = None;
    let render = {
        let rendered_all = rendered_all.clone();
        move |render: Render<'_>| {
            let first = *first_play_at.get_or_insert_with(|| {
                info!(
                    mach_ns = mach_time(render.play_at),
                    "first frame's play time"
                );
                render.play_at
            });
            let since_first = render.play_at - first;
            let start =
                (since_first.as_nanos() * u128::from(SAMPLE_RATE)).div_ceil(1_000_000_000) as u64;
            let [left, right] = render.channels;
            for i in 0..FRAMES_PER_PACKET {
                let frame = start + i as u64;
                [left[i], right[i]] = if frame < total {
                    options.frame(frame)
                } else {
                    [0.0; 2]
                };
            }
            if start + FRAMES_PER_PACKET as u64 >= total {
                rendered_all.store(true, Ordering::Release);
            }
        }
    };
    eprintln!("connecting to {id}");
    let stream = match protocol {
        Some(protocol) => Stream::start_with_protocol(browser, id, protocol, render),
        None => Stream::start(browser, id, render),
    };
    let mut last = Status::Connecting;
    loop {
        let status = stream.status();
        match &status {
            Status::Unreachable(reason) => return Err(format!("unreachable: {reason}")),
            Status::Lost(reason) => return Err(format!("lost: {reason}")),
            Status::Streaming(_) if !matches!(last, Status::Streaming(_)) => eprintln!("streaming"),
            _ => {}
        }
        last = status;
        if rendered_all.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Status::Streaming(counters) = last {
        eprintln!(
            "sent {} packets, {} retransmitted, {} bytes; playing out",
            counters.packets_sent, counters.packets_retransmitted, counters.bytes_sent
        );
    }
    stream.finish(Duration::ZERO).wait();
    eprintln!("done");
    Ok(())
}
