use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Owns periodic work whose progress must not depend on window redraws.
pub(super) struct Background {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Background {
    /// Once stopping is requested, `tick` must clean up and return false.
    pub fn spawn(
        name: &str,
        interval: Duration,
        mut tick: impl FnMut(bool) -> bool + Send + 'static,
    ) -> std::io::Result<Self> {
        let (stop, requests) = mpsc::channel();
        let thread = thread::Builder::new().name(name.into()).spawn(move || {
            let mut stopping = false;
            loop {
                let started = Instant::now();
                if !tick(stopping) {
                    break;
                }
                let remaining = interval.saturating_sub(started.elapsed());
                if stopping {
                    thread::sleep(remaining);
                } else {
                    stopping = !matches!(
                        requests.recv_timeout(remaining),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    );
                }
            }
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(&self) {
        let _ = self.stop.send(());
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn join(mut self) -> thread::Result<()> {
        self.stop();
        self.thread.take().unwrap().join()
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_without_rendering_and_joins_after_cleanup() {
        let (events, received) = mpsc::channel();
        let mut cleanup_ticks = 0;
        let worker = Background::spawn("background-test", Duration::from_millis(1), move |stop| {
            if stop {
                cleanup_ticks += 1;
            }
            events.send((stop, cleanup_ticks)).unwrap();
            cleanup_ticks < 2
        })
        .unwrap();
        for _ in 0..3 {
            assert_eq!(
                received.recv_timeout(Duration::from_secs(2)).unwrap(),
                (false, 0)
            );
        }
        assert!(!worker.is_finished());
        worker.stop();
        worker.join().unwrap();
        let remaining: Vec<_> = received.try_iter().collect();
        assert!(remaining.ends_with(&[(true, 1), (true, 2)]));
    }

    #[test]
    fn dropping_wakes_sleeping_worker_and_waits_for_stop() {
        let (events, received) = mpsc::channel();
        let worker = Background::spawn("background-drop", Duration::from_secs(30), move |stop| {
            events.send(stop).unwrap();
            !stop
        })
        .unwrap();
        assert!(!received.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(worker);
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap());
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
