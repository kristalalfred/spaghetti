use std::time::Duration;

use crate::airplay2::Failure;
use crate::rtsp::{Connection, Request, Response};
use crate::srp::{self, PROOF_LEN, PUBLIC_LEN};
use crate::tlv8::{self, Tlv};

pub(crate) const USER: &str = "Pair-Setup";
/// Claude: the PIN every AirPlay 2 receiver accepts for transient pairing.
pub(crate) const TRANSIENT_PIN: &str = "3939";
const SALT_LEN: usize = 16;
const PAIR_SETUP: u8 = 0;
const TRANSIENT: u8 = 0x10;
const M1: u8 = 1;
const M2: u8 = 2;
const M3: u8 = 3;
const M4: u8 = 4;

/// Claude: pairs transiently, as pyatv's hap_transient and pair_ap do: the
/// first four steps of HomeKit's pair-setup with the PIN 3939, after which the
/// SRP session key is the shared secret and no pair-verify follows. The
/// receiver's proof is checked before the secret is used.
pub(crate) fn pair_transiently(
    connection: &mut Connection,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<[u8; PROOF_LEN], Failure> {
    let m2 = step(
        connection,
        &[
            (tlv8::STATE, &[M1]),
            (tlv8::METHOD, &[PAIR_SETUP]),
            (tlv8::FLAGS, &[TRANSIENT]),
        ],
        M2,
        timeout,
        abort,
    )?;
    let salt = m2
        .get(tlv8::SALT)
        .filter(|salt| salt.len() == SALT_LEN)
        .ok_or_else(|| malformed("M2 has no 16-byte salt"))?;
    let server_public = m2
        .get(tlv8::PUBLIC_KEY)
        .filter(|public| !public.is_empty() && public.len() <= PUBLIC_LEN)
        .ok_or_else(|| malformed("M2 has no SRP public value"))?;
    let session = srp::client(USER, TRANSIENT_PIN, salt, server_public, || {
        let mut private = [0; srp::PRIVATE_LEN];
        getrandom::fill(&mut private).map_err(|e| format!("no randomness ({e})"))?;
        Ok(private)
    })
    .map_err(|reason| malformed(&reason))?;

    let m4 = step(
        connection,
        &[
            (tlv8::STATE, &[M3]),
            (tlv8::PUBLIC_KEY, &session.public),
            (tlv8::PROOF, &session.proof),
        ],
        M4,
        timeout,
        abort,
    )?;
    let proof = m4
        .get(tlv8::PROOF)
        .ok_or_else(|| malformed("M4 has no proof"))?;
    if !constant_time_eq(proof, &session.server_proof) {
        return Err(Failure::Failed(
            "its pairing proof is wrong, so it does not know the PIN it paired with".into(),
        ));
    }
    Ok(session.key)
}

fn step(
    connection: &mut Connection,
    items: &[(u8, &[u8])],
    expected_state: u8,
    timeout: Duration,
    abort: &dyn Fn() -> bool,
) -> Result<Tlv, Failure> {
    let body = Tlv::encode(items);
    let request = Request {
        method: "POST",
        uri: "/pair-setup",
        headers: vec![("X-Apple-HKP", "4".into())],
        body: Some(("application/octet-stream", &body)),
    };
    let response = connection
        .exchange(&request, timeout, abort)
        .map_err(Failure::Failed)?;
    reply(&response, expected_state)
}

fn reply(response: &Response, expected_state: u8) -> Result<Tlv, Failure> {
    let step = format!("M{}", expected_state - 1);
    if response.status != 200 {
        return Err(Failure::Refused(crate::session::refusal(
            &format!("transient pairing ({step})"),
            response,
        )));
    }
    let tlv = Tlv::decode(&response.body).map_err(|reason| malformed(&reason))?;
    if let Some(error) = tlv.get(tlv8::ERROR) {
        return Err(Failure::Refused(format!(
            "it refused transient pairing at {step} with {}",
            error_name(error)
        )));
    }
    match tlv.get(tlv8::STATE) {
        Some([state]) if *state == expected_state => Ok(tlv),
        state => Err(malformed(&format!(
            "its answer to {step} is in state {state:?}, not M{expected_state}"
        ))),
    }
}

fn error_name(error: &[u8]) -> String {
    match error {
        [1] => "an unknown error".into(),
        [2] => "an authentication error".into(),
        [3] => "a request to back off".into(),
        [4] => "too many paired senders".into(),
        [5] => "too many attempts".into(),
        [6] => "pairing unavailable".into(),
        [7] => "busy".into(),
        other => format!("error {other:02x?}"),
    }
}

fn malformed(reason: &str) -> Failure {
    Failure::Failed(format!("its transient pairing failed: {reason}"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: Vec<u8>) -> Response {
        Response {
            status,
            reason: if status == 200 { "OK" } else { "Refused" }.into(),
            headers: vec![],
            body,
        }
    }

    #[test]
    fn answers_are_checked_for_status_error_and_state() {
        let ok = |items: &[(u8, &[u8])]| reply(&response(200, Tlv::encode(items)), M2);
        assert!(ok(&[(tlv8::STATE, &[M2]), (tlv8::SALT, &[0; 16])]).is_ok());
        assert!(matches!(
            reply(&response(470, vec![]), M2),
            Err(Failure::Refused(reason)) if reason.contains("PIN")
        ));
        assert!(matches!(
            ok(&[(tlv8::STATE, &[M2]), (tlv8::ERROR, &[2])]),
            Err(Failure::Refused(reason)) if reason.contains("authentication")
        ));
        assert!(matches!(
            ok(&[(tlv8::STATE, &[M4])]),
            Err(Failure::Failed(reason)) if reason.contains("state")
        ));
        assert!(matches!(ok(&[]), Err(Failure::Failed(_))));
        assert!(matches!(
            reply(&response(200, vec![tlv8::STATE, 5, 2]), M2),
            Err(Failure::Failed(reason)) if reason.contains("claims 5 bytes")
        ));
    }

    #[test]
    fn proofs_compare_whole() {
        assert!(constant_time_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!constant_time_eq(&[1, 2], &[1, 2, 3]));
    }
}
