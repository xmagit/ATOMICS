//! In-process runtime wakeups. Producers signal work directly, independently of egui.

use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::app::State;
use crate::ipc::{ControlMessage, ControlOutcome};
use imacs_core::control::ControlRequest;

/// Owns runtime state and the thread that services it. The desktop is an
/// in-process client; closing the whole application still shuts down the host.
pub struct Host {
    state: Arc<Mutex<State>>,
    wake: Wake,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    commands: Sender<ControlMessage>,
}

impl Host {
    pub(crate) fn start(state: State, wake: Wake, ctx: egui::Context, commands: Sender<ControlMessage>) -> Self {
        let state = Arc::new(Mutex::new(state));
        let stopped = Arc::new(AtomicBool::new(false));
        let (runtime, signal, stop) = (state.clone(), wake.clone(), stopped.clone());
        let thread = std::thread::Builder::new()
            .name("imacs-runtime".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let deadline = {
                        let mut state = runtime.lock().unwrap();
                        if state.runtime_tick(&ctx) {
                            ctx.request_repaint();
                        }
                        state.runtime_deadline()
                    };
                    signal.wait_until(deadline);
                }
            })
            .expect("runtime thread");
        Self { state, wake, stopped, thread: Some(thread), commands }
    }

    /// Borrow the shared native view and apply synchronous UI actions. No
    /// runtime processing is performed by this borrow or by egui callbacks.
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn wake(&self) {
        self.wake.notify();
    }

    pub fn command(&self, request: ControlRequest) -> mpsc::Receiver<ControlOutcome> {
        let (reply, response) = mpsc::channel();
        let _ = self.commands.send(ControlMessage { request, reply });
        response
    }

    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("runtime thread panicked");
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Default)]
pub struct Wake(Arc<(Mutex<bool>, Condvar)>);

impl Wake {
    pub fn notify(&self) {
        let (pending, ready) = &*self.0;
        *pending.lock().unwrap() = true;
        ready.notify_one();
    }

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
