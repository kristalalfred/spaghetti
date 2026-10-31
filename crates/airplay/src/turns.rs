use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::ReceiverId;

const ABORT_CHECK: Duration = Duration::from_millis(20);

/// Claude: the order in which streams take their turns at each receiver: a
/// stream plays once every stream started before it to that receiver is done.
#[derive(Default)]
pub(crate) struct Turns {
    queues: Mutex<Queues>,
    changed: Condvar,
}

#[derive(Default)]
struct Queues {
    next_ticket: u64,
    waiting: HashMap<ReceiverId, VecDeque<u64>>,
}

impl Turns {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn take(self: &Arc<Self>, receiver: ReceiverId) -> Turn {
        let mut queues = self.lock();
        let ticket = queues.next_ticket;
        queues.next_ticket += 1;
        queues
            .waiting
            .entry(receiver.clone())
            .or_default()
            .push_back(ticket);
        Turn {
            turns: self.clone(),
            receiver,
            ticket,
        }
    }
}

/// Claude: a stream's place in line at a receiver, held until the stream is
/// done with it.
pub(crate) struct Turn {
    turns: Arc<Turns>,
    receiver: ReceiverId,
    ticket: u64,
}

impl Turn {
    /// Claude: waits until every earlier turn at the receiver is over; false
    /// when `abort` turned true first.
    pub(crate) fn wait(&self, abort: &dyn Fn() -> bool) -> bool {
        let mut queues = self.turns.lock();
        loop {
            if queues.waiting.get(&self.receiver).and_then(VecDeque::front) == Some(&self.ticket) {
                return true;
            }
            if abort() {
                return false;
            }
            queues = self
                .turns
                .changed
                .wait_timeout(queues, ABORT_CHECK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut queues = self.turns.lock();
        if let Some(queue) = queues.waiting.get_mut(&self.receiver) {
            queue.retain(|ticket| *ticket != self.ticket);
            if queue.is_empty() {
                queues.waiting.remove(&self.receiver);
            }
        }
        self.turns.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn turns_at_one_receiver_go_in_order_and_others_do_not_wait() {
        let turns = Arc::new(Turns::default());
        let move_2: ReceiverId = "C4:38:75:04:5E:80".parse().unwrap();
        let other: ReceiverId = "02:B5:11:B2:28:53".parse().unwrap();
        let first = turns.take(move_2.clone());
        let second = turns.take(move_2.clone());
        let third = turns.take(move_2);
        assert!(turns.take(other).wait(&|| false));
        assert!(first.wait(&|| false));

        let started = Instant::now();
        assert!(!second.wait(&|| started.elapsed() > Duration::from_millis(60)));
        drop(second);
        let waiter = thread::spawn(move || third.wait(&|| false));
        thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished());
        drop(first);
        assert!(waiter.join().unwrap());
    }
}
