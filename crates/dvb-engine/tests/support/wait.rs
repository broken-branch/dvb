use std::time::Duration;

pub struct Wait {
    poll: Duration,
    attempts: usize,
}

pub const FAST: Wait = Wait {
    poll: Duration::from_millis(50),
    attempts: 200,
};
pub const SLOW: Wait = Wait {
    poll: Duration::from_millis(100),
    attempts: 100,
};

impl Wait {
    pub async fn until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..self.attempts {
            if ready() {
                return;
            }
            // Poll in real time: on a paused clock a Tokio sleep lets the idle
            // runtime jump to its deadline while engine work awaits OS I/O,
            // spending the budget in virtual time. A blocking task also holds
            // auto-advance, so only an explicit advance moves the clock here.
            let poll = self.poll;
            tokio::task::spawn_blocking(move || std::thread::sleep(poll))
                .await
                .unwrap();
        }
        panic!("timed out waiting for {what}");
    }
}

/// OS process I/O must finish before a paused Tokio clock jumps to a timer.
/// A blocking task inhibits auto-advance; dropping the sender also releases it
/// if the supplied future is cancelled or panics. Explicit advance still works.
pub async fn io<F: std::future::Future>(future: F) -> F::Output {
    let (send, receive) = std::sync::mpsc::channel::<()>();
    let hold = tokio::task::spawn_blocking(move || {
        let _ = receive.recv();
    });
    let output = future.await;
    drop(send);
    hold.await.unwrap();
    output
}
