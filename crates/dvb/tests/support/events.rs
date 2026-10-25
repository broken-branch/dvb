use dvb_core::{Event, EventId};

/// The engine's whole event log.
pub fn all(e: &dvb_engine::Engine) -> Vec<Event> {
    e.store.after(EventId(0)).unwrap()
}
