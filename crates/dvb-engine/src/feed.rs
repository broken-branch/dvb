//! The event feed behind `GET /events`: the backlog from the store, a marker
//! where it ends, then new events as they are appended. A subscriber that falls
//! behind the broadcast buffer is caught up from the store, so it gets every
//! event once, in id order.

use crate::engine::{AppState, Fail};
use dvb_core::{Event, EventId};
use futures_util::stream::{self, Stream, StreamExt};
use std::collections::VecDeque;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

#[derive(Debug, Clone, PartialEq)]
pub enum FeedItem {
    Event(Box<Event>),
    /// The backlog is done; what follows happens now.
    Live,
}

struct Tail {
    e: AppState,
    rx: Receiver<Event>,
    last: EventId,
    pending: VecDeque<Event>,
}

/// Every event after `after`, then the live ones.
pub fn after(e: &AppState, after: EventId) -> Result<impl Stream<Item = FeedItem> + use<>, Fail> {
    // Subscribe before reading the backlog so nothing falls between the two.
    let rx = e.subscribe();
    let backlog = e.store.after(after)?;
    let last = backlog.last().map_or(after, |ev| ev.id);
    let tail = Tail {
        e: e.clone(),
        rx,
        last,
        pending: VecDeque::new(),
    };
    let live = stream::unfold(tail, |mut t| async move {
        loop {
            if let Some(ev) = t.pending.pop_front() {
                t.last = ev.id;
                return Some((FeedItem::Event(Box::new(ev)), t));
            }
            match t.rx.recv().await {
                Ok(ev) if ev.id > t.last => {
                    t.last = ev.id;
                    return Some((FeedItem::Event(Box::new(ev)), t));
                }
                Ok(_) => {}
                // Fell behind the buffer: read what was missed from the store.
                Err(RecvError::Lagged(_)) => match t.e.store.after(t.last) {
                    Ok(missed) => t.pending.extend(missed),
                    Err(_) => return None,
                },
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Ok(
        stream::iter(backlog.into_iter().map(|ev| FeedItem::Event(Box::new(ev))))
            .chain(stream::once(async { FeedItem::Live }))
            .chain(live),
    )
}
