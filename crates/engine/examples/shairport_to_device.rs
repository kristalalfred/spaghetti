//! Claude: plays what a shairport-sync receiver writes to its standard output
//! on a spaghetti virtual device, each frame when the receiver planned to
//! play it, for the silent calibration check.
//!
//! Claude: shairport-sync's stdout backend writes frames as they arrive, about
//! two seconds before they play, behind as much silence as the first frame's
//! lead time. Its log at verbosity 3 gives that lead time and, as `pffr`, when
//! it plans to play the first frame, in host nanoseconds; frame k of its
//! output then plays `(k - lead frames) / 44100` s after that.

#[cfg(target_os = "macos")]
fn main() {
    if let Err(reason) = helper::run() {
        eprintln!("shairport_to_device: {reason}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("shairport_to_device: needs macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod helper {
    use std::io::{BufRead, BufReader, Read};
    use std::time::Duration;

    use spaghetti_engine::{Destination, Frames, HostTime, Router, SourceSpec};

    const RATE: f64 = 44_100.0;
    const BLOCK: usize = 352;
    const DELAY_MS: u32 = 500;
    const USAGE: &str = "usage: shairport_to_device DEVICE_UID SHAIRPORT_LOG < PCM";

    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let [uid, log] = &args[..] else {
            return Err(USAGE.into());
        };
        if !uid.starts_with("spaghetti_virtual_") {
            return Err(format!(
                "{uid} is not a spaghetti virtual device; refusing to play"
            ));
        }
        spaghetti_engine::find_device_by_uid(uid).ok_or(format!("device {uid} not found"))?;

        let mut router = Router::new();
        let mut input = router.add_program_input("shairport", RATE)?;
        let source = router.create_source(
            SourceSpec::Program {
                input: "shairport".into(),
            },
            None,
        )?;
        router.create_route(
            source,
            Destination::Device {
                uid: uid.clone(),
                delay_ms: Some(DELAY_MS),
                latency_offset_ms: None,
            },
            1.0,
        )?;
        let routed = router.list_routes();
        if !matches!(&routed[..], [route] if route.destination == Destination::Device {
            uid: uid.clone(),
            delay_ms: Some(DELAY_MS),
            latency_offset_ms: None,
        }) {
            return Err(format!("the route did not open on {uid}"));
        }
        eprintln!("shairport_to_device: playing on {uid}");

        let mut stdin = std::io::stdin().lock();
        let mut pcm = Vec::new();
        let mut received = 0usize;
        let (first_frame_ns, lead_frames) = loop {
            if let Some(planned) = planned(log)? {
                break planned;
            }
            let mut chunk = [0u8; BLOCK * 4];
            let read = stdin.read(&mut chunk).map_err(|e| e.to_string())?;
            if read == 0 {
                return Ok(());
            }
            pcm.extend_from_slice(&chunk[..read]);
            std::thread::sleep(Duration::from_millis(1));
        };
        let ticks_per_ns = {
            let probe = HostTime::now();
            let later = probe + Duration::from_secs(1);
            (later.ticks() - probe.ticks()) as f64 / 1e9
        };
        let delay_ticks = f64::from(DELAY_MS) / 1000.0 * 1e9 * ticks_per_ns;
        let first_tick = first_frame_ns as f64 * ticks_per_ns;
        let mut push = |samples: &[i16], frame: usize| -> Result<(), String> {
            let frames: Vec<f32> = samples.iter().map(|s| f32::from(*s) / 32768.0).collect();
            let plays =
                first_tick + (frame as f64 - lead_frames as f64) / RATE * 1e9 * ticks_per_ns;
            let captured = HostTime::from_ticks((plays - delay_ticks).max(0.0) as u64);
            input.push(
                Frames::interleaved(&frames).map_err(|e| e.to_string())?,
                captured,
            );
            Ok(())
        };
        loop {
            while pcm.len() >= BLOCK * 4 {
                let block: Vec<i16> = pcm[..BLOCK * 4]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                push(&block, received)?;
                received += BLOCK;
                pcm.drain(..BLOCK * 4);
            }
            let mut chunk = [0u8; BLOCK * 4];
            let read = stdin.read(&mut chunk).map_err(|e| e.to_string())?;
            if read == 0 {
                return Ok(());
            }
            pcm.extend_from_slice(&chunk[..read]);
        }
    }

    /// Claude: when shairport-sync plans to play the session's first frame, in
    /// host nanoseconds, and how many frames of silence its output leads with,
    /// once its log says both.
    fn planned(log: &str) -> Result<Option<(u64, usize)>, String> {
        let file = std::fs::File::open(log).map_err(|e| format!("{log}: {e}"))?;
        let (mut first, mut lead) = (None, None);
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if let Some(at) = line.find("lead time for first frame ") {
                let seconds = line[at..]
                    .split(": ")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|seconds| seconds.parse::<f64>().ok());
                lead = seconds.map(|seconds| (seconds * RATE).round() as usize);
            }
            if let Some(at) = line.find("pffr: \"") {
                first = line[at + 7..]
                    .split(['/', '"'])
                    .nth(1)
                    .and_then(|ns| ns.parse::<u64>().ok());
            }
        }
        Ok(first.zip(lead))
    }
}
