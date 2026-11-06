//! Claude: `spaghetti calibrate`, run in this process with an engine of its
//! own, so the microphone is opened by the terminal's process and no daemon
//! is needed.

use std::error::Error;
use std::io::{ErrorKind, IsTerminal};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use clap::Args;
use serde_json::{Value, json};
use spaghetti_common::ipc_protocol::{
    AirPlayReceiverInfo, Destination, Request, Response, RouteInfo, SOCKET_PATH,
};
use spaghetti_engine::{
    Calibrated, Calibrating, Calibration, CalibrationFailure, CalibrationState, Recording, Router,
};

use crate::output::Output;
use crate::{SrtArgs, client, route_destination};

/// Claude: how long a running daemon may take to say what it routes.
const PREFLIGHT: Duration = Duration::from_secs(2);

#[derive(Args)]
pub struct CalibrateArgs {
    #[arg(help = "Device UID, airplay://NAME or airplay://ID to calibrate")]
    destination: String,
    #[arg(
        long,
        help = "Device UID, airplay://NAME or airplay://ID to sound with"
    )]
    reference: String,
    #[arg(
        long,
        value_name = "UID",
        help = "Input device that hears both [default: the default input]"
    )]
    microphone: Option<String>,
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 3000,
        help = "Delay the destination plays on"
    )]
    delay: u32,
    #[arg(
        long = "latency-offset",
        value_name = "MS",
        allow_negative_numbers = true,
        help = "Latency offset the destination plays with"
    )]
    latency_offset_ms: Option<i32>,
    #[arg(
        long = "reference-delay",
        value_name = "MS",
        help = "[default: --delay]"
    )]
    reference_delay: Option<u32>,
    #[arg(
        long = "reference-latency-offset",
        value_name = "MS",
        allow_negative_numbers = true,
        help = "Latency offset the reference plays with"
    )]
    reference_latency_offset_ms: Option<i32>,
}

fn receivers() -> Result<Vec<AirPlayReceiverInfo>, String> {
    spaghetti_engine::list_airplay_receivers()
}

pub fn run(args: CalibrateArgs, out: Output) -> Result<(), Box<dyn Error>> {
    log_to_stderr();
    let plain = SrtArgs {
        latency: None,
        passphrase: None,
    };
    let output = route_destination(
        &args.destination,
        Some(args.delay),
        args.latency_offset_ms,
        &plain,
        receivers,
    )?;
    let reference = route_destination(
        &args.reference,
        Some(args.reference_delay.unwrap_or(args.delay)),
        args.reference_latency_offset_ms,
        &plain,
        receivers,
    )?;
    preflight([&output, &reference], Path::new(SOCKET_PATH))?;
    let microphone = match args.microphone {
        Some(uid) => uid,
        None => spaghetti_engine::default_input_device()
            .ok_or("no default input device; name one with --microphone")?,
    };
    let mut router = Router::new();
    let calibrating = router.calibrate(Calibration {
        output: output.into(),
        reference: reference.into(),
        recording: Recording::Input {
            device_uid: microphone,
        },
    })?;
    match wait(&calibrating, out)? {
        Ok(calibrated) => {
            let names = [args.destination.as_str(), args.reference.as_str()];
            out.emit(&as_json(names, &calibrated), || {
                print!("{}", format_calibrated(names, &calibrated))
            })?;
            Ok(())
        }
        Err(CalibrationFailure::Cancelled) => std::process::exit(130),
        Err(failure) => Err(failure.to_string().into()),
    }
}

/// Claude: the engine runs in this process, so what it warns of goes to
/// standard error, beside the command's own output; `RUST_LOG` says more.
fn log_to_stderr() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

/// Claude: refuses to calibrate an AirPlay receiver the daemon listening on
/// `socket` plays to. Without an AirPlay destination it asks nothing; with
/// one, a daemon that does not answer within `PREFLIGHT`, or answers badly,
/// refuses it too.
fn preflight(destinations: [&Destination; 2], socket: &Path) -> Result<(), String> {
    let airplay = destinations
        .iter()
        .any(|destination| matches!(destination, Destination::AirPlay { .. }));
    if !airplay {
        return Ok(());
    }
    refuse_receivers_in_use(destinations, &daemon_routes(socket)?)
}

/// Claude: what the daemon listening on `socket` routes, or nothing when no
/// daemon listens there.
fn daemon_routes(socket: &Path) -> Result<Vec<RouteInfo>, String> {
    let unanswered = |reason: String| {
        format!(
            "the daemon did not say whether it plays to the AirPlay receiver ({reason}); \
             try again, or stop it with: spaghetti daemon stop"
        )
    };
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(unanswered(e.to_string())),
    };
    stream
        .set_read_timeout(Some(PREFLIGHT))
        .and_then(|()| stream.set_write_timeout(Some(PREFLIGHT)))
        .map_err(|e| unanswered(e.to_string()))?;
    match client::request(&mut stream, &Request::ListRoutes) {
        Ok(Response::Routes(routes)) => Ok(routes),
        Ok(_) => Err(unanswered(client::UNEXPECTED.into())),
        Err(reason) => Err(unanswered(reason)),
    }
}

/// Claude: an AirPlay receiver plays one sender at a time, so one the daemon
/// plays to cannot be calibrated.
fn refuse_receivers_in_use(
    destinations: [&Destination; 2],
    routes: &[RouteInfo],
) -> Result<(), String> {
    let id = |receiver: &str| {
        receiver
            .parse::<airplay::ReceiverId>()
            .map_or_else(|_| receiver.to_string(), |id| id.to_string())
    };
    for destination in destinations {
        let Destination::AirPlay { receiver, .. } = destination else {
            continue;
        };
        let used = routes.iter().find(|route| {
            matches!(&route.destination, Destination::AirPlay { receiver: other, .. } if id(other) == id(receiver))
        });
        if let Some(route) = used {
            return Err(format!(
                "the daemon's route {} plays to AirPlay receiver {receiver}; remove it with: spaghetti unroute {}",
                route.route_id, route.route_id
            ));
        }
    }
    Ok(())
}

/// Claude: polls the calibration until it is done, showing each state it
/// reaches on standard error. Ctrl-C cancels it, and it is done once it has
/// released what it played and recorded through.
fn wait(
    calibrating: &Calibrating,
    out: Output,
) -> Result<Result<Calibrated, CalibrationFailure>, Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(async {
        let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
        let mut interrupted = false;
        let mut shown = None;
        loop {
            let state = calibrating.state();
            if let CalibrationState::Done(result) = state {
                return result;
            }
            if out == Output::Text && shown.as_ref() != Some(&state) {
                eprintln!("{}", progress(&state));
                shown = Some(state);
            }
            let poll = tokio::time::sleep(Duration::from_millis(50));
            if interrupted {
                poll.await;
                continue;
            }
            tokio::select! {
                _ = &mut interrupt => {
                    interrupted = true;
                    calibrating.cancel();
                }
                () = poll => {}
            }
        }
    }))
}

fn progress(state: &CalibrationState) -> String {
    match state {
        CalibrationState::Waiting { role, train } => format!("{role}: train {train}, waiting"),
        CalibrationState::Probing { role, train } => format!("{role}: train {train}, probing"),
        CalibrationState::Analyzing => "analyzing".into(),
        CalibrationState::Done(_) => "done".into(),
    }
}

fn format_calibrated([output, reference]: [&str; 2], calibrated: &Calibrated) -> String {
    let width = output.chars().count().max(reference.chars().count());
    let row = |name: &str, residual: spaghetti_engine::Residual| {
        format!(
            "{name:<width$}  {:>+9.2} ms  ±{:.2} ms\n",
            residual.late_ms, residual.spread_ms
        )
    };
    format!(
        "{}{}--latency-offset {}\n",
        row(output, calibrated.output),
        row(reference, calibrated.reference),
        calibrated.latency_offset_ms
    )
}

fn as_json([output, reference]: [&str; 2], calibrated: &Calibrated) -> Value {
    let residual = |destination: &str, residual: spaghetti_engine::Residual| {
        json!({
            "destination": destination,
            "late_ms": residual.late_ms,
            "spread_ms": residual.spread_ms,
        })
    };
    json!({
        "output": residual(output, calibrated.output),
        "reference": residual(reference, calibrated.reference),
        "latency_offset_ms": calibrated.latency_offset_ms,
    })
}

#[cfg(test)]
mod tests {
    use spaghetti_common::ipc_protocol::{FlowState, FlowStatus};
    use spaghetti_engine::Residual;

    use super::*;

    fn route(route_id: u32, destination: Destination) -> RouteInfo {
        RouteInfo {
            route_id,
            source_id: 1,
            destination,
            volume: 1.0,
            status: FlowStatus {
                state: FlowState::Flowing,
                bytes: 0,
                peer: None,
                relayed: false,
                packets_lost: 0,
                packets_retransmitted: 0,
            },
            receiver_volume: None,
            local_description: None,
        }
    }

    fn airplay(receiver: &str) -> Destination {
        Destination::AirPlay {
            receiver: receiver.into(),
            delay_ms: 3000,
            latency_offset_ms: None,
        }
    }

    #[test]
    fn a_receiver_the_daemon_plays_to_is_refused() {
        let device = Destination::Device {
            uid: "Speakers".into(),
            delay_ms: Some(3000),
            latency_offset_ms: None,
        };
        let routes = [
            route(4, airplay("C4:38:75:04:5E:80")),
            route(5, device.clone()),
        ];
        let refused =
            refuse_receivers_in_use([&airplay("c4:38:75:04:5e:80"), &device], &routes).unwrap_err();
        assert!(refused.contains("spaghetti unroute 4"), "{refused}");
        assert!(refuse_receivers_in_use([&airplay("C4:38:75:04:5E:81"), &device], &routes).is_ok());
    }

    /// Claude: a socket path of its own in the temporary directory, removed
    /// when dropped.
    struct Socket(std::path::PathBuf);

    impl Socket {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "spaghetti-preflight-{}-{name}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        /// Claude: a daemon that answers each request with `answer`'s bytes,
        /// or never answers when there are none.
        fn daemon(&self, answer: Option<Vec<u8>>) -> std::thread::JoinHandle<()> {
            use std::io::Write;
            use std::os::unix::net::UnixListener;

            let listener = UnixListener::bind(&self.0).unwrap();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _: Request = spaghetti_common::ipc_protocol::read_message(&mut stream).unwrap();
                match answer {
                    Some(bytes) => stream.write_all(&bytes).unwrap(),
                    None => std::thread::sleep(PREFLIGHT + Duration::from_secs(1)),
                }
            })
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn message(response: &Response) -> Vec<u8> {
        let mut bytes = Vec::new();
        spaghetti_common::ipc_protocol::write_message(&mut bytes, response).unwrap();
        bytes
    }

    fn device() -> Destination {
        Destination::Device {
            uid: "Speakers".into(),
            delay_ms: Some(3000),
            latency_offset_ms: None,
        }
    }

    #[test]
    fn no_daemon_lets_a_receiver_be_calibrated() {
        let receiver = airplay("C4:38:75:04:5E:80");
        let missing = Socket::new("missing");
        assert_eq!(preflight([&receiver, &device()], &missing.0), Ok(()));
        let stale = Socket::new("stale");
        drop(std::os::unix::net::UnixListener::bind(&stale.0).unwrap());
        assert!(stale.0.exists());
        assert_eq!(preflight([&receiver, &device()], &stale.0), Ok(()));
    }

    #[test]
    fn a_daemon_that_answers_late_or_badly_refuses_a_receivers_calibration() {
        let receiver = airplay("C4:38:75:04:5E:80");
        let answers = [
            ("late", None),
            ("garbled", Some(b"\0\0\0\x05nope!".to_vec())),
            ("endless", Some(vec![0xFF; 4])),
            ("other", Some(message(&Response::Ok))),
        ];
        for (name, answer) in answers {
            let socket = Socket::new(name);
            let daemon = socket.daemon(answer);
            let started = std::time::Instant::now();
            let refused = preflight([&receiver, &device()], &socket.0).unwrap_err();
            let took = started.elapsed();
            assert!(refused.contains("did not say"), "{name}: {refused}");
            assert!(
                took < PREFLIGHT + Duration::from_millis(500),
                "{name}: {took:?}"
            );
            daemon.join().unwrap();
        }
    }

    #[test]
    fn a_daemon_is_asked_only_about_a_receiver_and_its_answer_is_used() {
        let socket = Socket::new("asked");
        let _never_accepting = std::os::unix::net::UnixListener::bind(&socket.0).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(preflight([&device(), &device()], &socket.0), Ok(()));
        assert!(started.elapsed() < Duration::from_millis(100));

        let routes = vec![route(4, airplay("C4:38:75:04:5E:80"))];
        let socket = Socket::new("routes");
        let daemon = socket.daemon(Some(message(&Response::Routes(routes))));
        let refused = preflight([&device(), &airplay("c4:38:75:04:5e:80")], &socket.0);
        assert!(refused.unwrap_err().contains("spaghetti unroute 4"));
        daemon.join().unwrap();
    }

    #[test]
    fn a_result_shows_both_residuals_and_the_offset_to_use() {
        let calibrated = Calibrated {
            output: Residual {
                late_ms: 215.4,
                spread_ms: 0.2,
            },
            reference: Residual {
                late_ms: 3.1,
                spread_ms: 0.1,
            },
            latency_offset_ms: 212,
        };
        let names = ["airplay://Kitchen", "BuiltInSpeakerDevice"];
        assert_eq!(
            format_calibrated(names, &calibrated),
            "airplay://Kitchen       +215.40 ms  ±0.20 ms\n\
             BuiltInSpeakerDevice      +3.10 ms  ±0.10 ms\n\
             --latency-offset 212\n"
        );
        let json = as_json(names, &calibrated);
        assert_eq!(json["latency_offset_ms"], 212);
        assert_eq!(json["output"]["destination"], "airplay://Kitchen");
        assert_eq!(json["reference"]["late_ms"], 3.1);
    }
}
