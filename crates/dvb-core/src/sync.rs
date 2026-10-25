//! Mutex poisoning policy shared by the engine and its dependencies.
use std::sync::PoisonError;

/// Recover the protected value after a panicking holder. Durable operations
/// remain transactional; poisoning must not permanently disable the process.
pub fn recover<T>(error: PoisonError<T>) -> T {
    error.into_inner()
}

#[cfg(test)]
mod tests {
    #[test]
    #[expect(
        clippy::significant_drop_tightening,
        reason = "The guard must remain held while unwinding to poison the mutex"
    )]
    fn a_panicking_holder_does_not_disable_future_locking() {
        let mutex = std::sync::Mutex::new(1);
        let _ = std::panic::catch_unwind(|| {
            let mut value = mutex.lock().unwrap();
            *value = 2;
            panic!("holder value {}", *value);
        });
        *mutex.lock().unwrap_or_else(super::recover) += 1;
        assert_eq!(*mutex.lock().unwrap_or_else(super::recover), 3);
    }
}
