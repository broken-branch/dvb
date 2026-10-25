//! A subscriber that falls behind the broadcast buffer still gets every event,
//! once, in order.

mod support;
use dvb_core::{Actor, EventId, EventKind};
use dvb_engine::feed::{self, FeedItem};
use futures_util::StreamExt;

#[tokio::test]
async fn a_lagging_subscriber_gets_every_event_in_order() {
    let (_dir, e) = support::engine::engine();
    let stream = feed::after(&e, EventId(0)).unwrap();
    // More than the broadcast buffer holds, while nobody reads.
    for _ in 0..1_500 {
        e.append_checked(None, Actor::Owner, |_, _| {
            Ok(vec![EventKind::ProjectResumed {}])
        })
        .unwrap();
    }
    let last = e.store.last_id().unwrap();
    let ids: Vec<u64> = stream
        .filter_map(|item| async move {
            match item {
                FeedItem::Event(ev) => Some(ev.id.0),
                FeedItem::Live => None,
            }
        })
        .take_while(|id| futures_util::future::ready(*id <= last.0))
        .take(usize::try_from(last.0).unwrap())
        .collect()
        .await;
    let want: Vec<u64> = (1..=last.0).collect();
    assert_eq!(ids, want);
}
