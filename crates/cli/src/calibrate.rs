//! Claude: `spaghetti calibrate`, run in this process with an engine of its
//! own, so the microphone is opened by the terminal's process and no daemon
//! is needed.

use std::error::Error;
use std::os::unix::net::UnixStream;
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

/// Claude: how long a running daemon may take to list its routes before the
/// command goes on without asking it.
const PREFLIGHT: Duration = Duration::from_millis(250);

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
    refuse_receivers_in_use([&output, &reference], &daemon_routes())?;
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

/// Claude: what a running daemon routes, or nothing when none answers within
/// `PREFLIGHT`.
fn daemon_routes() -> Vec<RouteInfo> {
    let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) else {
        return Vec::new();
    };
    let bounded = stream
        .set_read_timeout(Some(PREFLIGHT))
        .and_then(|()| stream.set_write_timeout(Some(PREFLIGHT)));
    match bounded.map(|()| client::request(&mut stream, &Request::ListRoutes)) {
        Ok(Ok(Response::Routes(routes))) => routes,
        _ => Vec::new(),
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
        CalibrationState::Waiting(role) => format!("{role}: starting"),
        CalibrationState::Probing(role) => format!("{role}: probing"),
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
