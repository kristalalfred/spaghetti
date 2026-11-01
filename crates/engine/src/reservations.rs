//! Claude: endpoints a calibration holds, which no route plays to meanwhile.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    AirPlay(String),
    ProgramOutput(String),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AirPlay(receiver) => write!(f, "AirPlay receiver '{receiver}'"),
            Self::ProgramOutput(output) => write!(f, "program output '{output}'"),
        }
    }
}

#[derive(Default)]
pub struct Reservations(Mutex<Vec<Endpoint>>);

impl Reservations {
    fn held(&self) -> MutexGuard<'_, Vec<Endpoint>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn check(&self, endpoint: &Endpoint) -> Result<(), String> {
        if self.held().contains(endpoint) {
            return Err(format!("{endpoint} is being calibrated"));
        }
        Ok(())
    }

    /// Claude: all of `endpoints` or none, held until the result drops.
    pub fn reserve(self: &Arc<Self>, endpoints: Vec<Endpoint>) -> Result<Reservation, String> {
        let mut held = self.held();
        if let Some(taken) = endpoints.iter().find(|endpoint| held.contains(endpoint)) {
            return Err(format!("{taken} is being calibrated"));
        }
        held.extend(endpoints.iter().cloned());
        Ok(Reservation {
            reservations: Arc::clone(self),
            endpoints,
        })
    }
}

pub struct Reservation {
    reservations: Arc<Reservations>,
    endpoints: Vec<Endpoint>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut held = self.reservations.held();
        for endpoint in &self.endpoints {
            if let Some(at) = held.iter().position(|h| h == endpoint) {
                held.swap_remove(at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_is_held_until_its_reservation_drops() {
        let reservations = Arc::new(Reservations::default());
        let output = Endpoint::ProgramOutput("speaker".into());
        let receiver = Endpoint::AirPlay("C4:38:75:04:5E:80".into());
        let held = reservations.reserve(vec![output.clone()]).unwrap();
        assert!(reservations.check(&output).is_err());
        assert!(reservations.check(&receiver).is_ok());
        assert!(
            reservations
                .reserve(vec![receiver.clone(), output.clone()])
                .is_err()
        );
        assert!(reservations.check(&receiver).is_ok(), "reserved in part");
        drop(held);
        assert!(reservations.check(&output).is_ok());
    }
}
