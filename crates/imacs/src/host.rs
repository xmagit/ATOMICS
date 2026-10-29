//! In-process runtime wakeups. Producers signal work directly, independently of egui.

use std::sync::{Arc, Condvar, Mutex, mpsc};
#[cfg(test)]
use std::time::Instant;

#[derive(Clone, Default)]
pub struct Wake(Arc<(Mutex<bool>, Condvar)>);

impl Wake {
    pub fn notify(&self) {
        let (pending, ready) = &*self.0;
        *pending.lock().unwrap() = true;
        ready.notify_one();
    }

    #[cfg(test)]
    pub fn wait_until(&self, deadline: Instant) {
        let (pending, ready) = &*self.0;
        let mut pending = pending.lock().unwrap();
        while !*pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            pending = ready.wait_timeout(pending, remaining).unwrap().0;
        }
        *pending = false;
    }

    pub fn channel<T>(&self) -> (Sender<T>, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel();
        (Sender { tx, wake: self.clone() }, rx)
    }
}

pub struct Sender<T> {
    tx: mpsc::Sender<T>,
    wake: Wake,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), wake: self.wake.clone() }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        self.tx.send(value)?;
        self.wake.notify();
        Ok(())
    }
}

#[cfg(test)]
impl<T> From<mpsc::Sender<T>> for Sender<T> {
    fn from(tx: mpsc::Sender<T>) -> Self {
        Self { tx, wake: Wake::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn messages_wake_a_sleeping_runtime_without_losing_an_early_signal() {
        let wake = Wake::default();
        let (tx, rx) = wake.channel();
        tx.send(1).unwrap();
        let started = Instant::now();
        wake.wait_until(started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.recv().unwrap(), 1);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            tx.send(2).unwrap();
        });
        let started = Instant::now();
        wake.wait_until(started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.recv().unwrap(), 2);
        producer.join().unwrap();
    }

    #[test]
    fn runtime_wakes_at_its_deadline_without_a_producer() {
        let wake = Wake::default();
        let deadline = Instant::now() + Duration::from_millis(20);
        wake.wait_until(deadline);
        assert!(Instant::now() >= deadline);
    }
}
