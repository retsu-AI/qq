//! The published-event outbox and per-workspace live ring.
//!
//! Every committed event is serialized exactly once, inside the store
//! transaction that persists it. That encoding is kept as a [`PublishedEvent`]
//! and appended to the workspace's bounded, sequence-indexed ring after the
//! transaction commits. Subscribers read the ring by cursor: a subscriber in
//! steady state performs no store read and no parse per event, a reconnecting
//! subscriber whose cursor is still inside the ring catches up from memory
//! without a store round trip, and the server writes the same bytes to the
//! wire. SQLite remains authoritative: a subscriber catches up from it whenever
//! its cursor is behind the ring (cold attach, or after lagging).

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    pin::pin,
    sync::{Arc, Mutex},
};

use qq_protocol::{SessionEventEnvelope, WorkspaceId};
use tokio::sync::Notify;

/// Live events retained per workspace for subscribers that keep up or
/// reconnect promptly. A subscriber that falls this far behind is redirected
/// to SQLite catch-up, so the bound never drops an event; it only bounds memory.
pub(super) const FEED_CAPACITY: usize = 1024;

/// Encoded bytes retained per workspace ring. Large streams would otherwise
/// pin roughly two copies of their recent payload (JSON plus the decoded
/// envelope) for warm replay; past this bound the oldest events are released
/// and a subscriber that needs them catches up from SQLite.
pub(super) const FEED_RETAINED_BYTES: usize = 256 * 1024;

/// One committed event with its canonical JSON encoding.
///
/// `json` is the exact string persisted in `events.envelope_json`, so a live
/// delivery and a replayed one are byte-identical.
#[derive(Debug)]
pub struct PublishedEvent {
    pub envelope: SessionEventEnvelope,
    pub json: Arc<str>,
}

impl PublishedEvent {
    /// The envelope, moving it out when this is the last reference and
    /// cloning otherwise. A single subscriber pays no clone.
    pub fn into_envelope(this: Arc<Self>) -> SessionEventEnvelope {
        match Arc::try_unwrap(this) {
            Ok(published) => published.envelope,
            Err(shared) => shared.envelope.clone(),
        }
    }

    fn sequence(&self) -> u64 {
        self.envelope.cursor.sequence
    }
}

thread_local! {
    /// Events appended by the store job currently running on this thread.
    /// The database worker runs one job at a time, so between `take` calls
    /// this holds exactly one transaction's appends.
    static OUTBOX: RefCell<Vec<Arc<PublishedEvent>>> = const { RefCell::new(Vec::new()) };
}

/// Records one event the current transaction appended.
pub(super) fn stage(event: Arc<PublishedEvent>) {
    OUTBOX.with(|outbox| outbox.borrow_mut().push(event));
}

/// Drains everything the job that just ran appended. Called by the worker
/// after the job returns: on success the batch is published, on failure it is
/// discarded because the transaction rolled back.
pub(super) fn take_staged() -> Vec<Arc<PublishedEvent>> {
    OUTBOX.with(|outbox| std::mem::take(&mut *outbox.borrow_mut()))
}

/// Why a live read could not return the next event.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecvError {
    /// The ring no longer holds `after + 1`; the caller must catch up from
    /// the store and then resume.
    Behind,
    /// The ring registry is unusable.
    Closed,
}

enum Lookup {
    Ready(Arc<PublishedEvent>),
    Behind,
    CaughtUp,
}

/// One workspace's most recent committed events, contiguous by sequence.
struct Ring {
    events: VecDeque<Arc<PublishedEvent>>,
    /// Newest sequence known committed for the workspace, kept even while
    /// `events` is empty so a subscriber at that cursor attaches warm.
    tail: Option<u64>,
    /// Sum of `json.len()` over `events`.
    retained_bytes: usize,
    subscribers: usize,
    notify: Arc<Notify>,
}

impl Ring {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            tail: None,
            retained_bytes: 0,
            subscribers: 0,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Whether a subscriber at `after` can be served from memory: every
    /// event past its cursor is retained, or its cursor is the known tail.
    fn covers(&self, after: u64) -> bool {
        match (self.events.front(), self.tail) {
            (Some(first), Some(tail)) => after + 1 >= first.sequence() && after <= tail,
            (None, Some(tail)) => after == tail,
            (_, None) => false,
        }
    }

    fn lookup(&self, after: u64) -> Lookup {
        match self.events.front() {
            Some(first) if after + 1 < first.sequence() => Lookup::Behind,
            Some(first) => match usize::try_from(after + 1 - first.sequence())
                .ok()
                .and_then(|index| self.events.get(index))
            {
                Some(event) => Lookup::Ready(Arc::clone(event)),
                None => Lookup::CaughtUp,
            },
            None => match self.tail {
                Some(tail) if after < tail => Lookup::Behind,
                _ => Lookup::CaughtUp,
            },
        }
    }

    /// Up to `limit` retained events after `after`. Callers check `covers`
    /// first; an uncovered cursor yields an empty page.
    fn page(&self, after: u64, limit: u16) -> Vec<Arc<PublishedEvent>> {
        let Some(first) = self.events.front() else {
            return Vec::new();
        };
        if after + 1 < first.sequence() {
            return Vec::new();
        }
        let Ok(start) = usize::try_from(after + 1 - first.sequence()) else {
            return Vec::new();
        };
        if start >= self.events.len() {
            return Vec::new();
        }
        self.events
            .range(start..)
            .take(usize::from(limit))
            .cloned()
            .collect()
    }

    fn push(&mut self, event: Arc<PublishedEvent>) {
        let sequence = event.sequence();
        // Sequences are allocated inside the committing transaction and
        // published on the one worker thread in commit order, so they are
        // contiguous. Should that ever fail, the ring must not serve a page
        // with a hole: discard it and let subscribers recover from SQLite.
        if self
            .events
            .back()
            .is_some_and(|last| last.sequence() + 1 != sequence)
        {
            self.events.clear();
            self.retained_bytes = 0;
        }
        self.retained_bytes += event.json.len();
        self.events.push_back(event);
        while self.events.len() > FEED_CAPACITY
            || (self.retained_bytes > FEED_RETAINED_BYTES && self.events.len() > 1)
        {
            if let Some(evicted) = self.events.pop_front() {
                self.retained_bytes -= evicted.json.len();
            }
        }
        self.tail = Some(sequence);
    }
}

/// Per-workspace rings of committed events, retained only while subscribed.
#[derive(Default)]
pub(super) struct WorkspaceFeed {
    rings: Mutex<HashMap<WorkspaceId, Ring>>,
}

/// One subscription. Reads are by cursor, so the receiver carries no position
/// of its own; dropping the last receiver releases the workspace ring.
pub(super) struct FeedReceiver {
    feed: Arc<WorkspaceFeed>,
    workspace_id: WorkspaceId,
    notify: Arc<Notify>,
}

impl FeedReceiver {
    /// The event at `after + 1`, waiting for it to be published if the
    /// subscriber is caught up. Cancel-safe: nothing is consumed until the
    /// event is returned.
    pub(super) async fn recv(&self, after: u64) -> Result<Arc<PublishedEvent>, RecvError> {
        // Fast path: the event is usually already in the ring, so avoid
        // constructing and registering a waiter for it.
        match self.feed.lookup(self.workspace_id, after) {
            Some(Lookup::Ready(event)) => return Ok(event),
            Some(Lookup::Behind) => return Err(RecvError::Behind),
            Some(Lookup::CaughtUp) => {}
            None => return Err(RecvError::Closed),
        }
        loop {
            let mut notified = pin!(self.notify.notified());
            // Register before reading the ring so a publish between the read
            // and the await wakes this waiter instead of being missed.
            notified.as_mut().enable();
            match self.feed.lookup(self.workspace_id, after) {
                Some(Lookup::Ready(event)) => return Ok(event),
                Some(Lookup::Behind) => return Err(RecvError::Behind),
                Some(Lookup::CaughtUp) => {}
                None => return Err(RecvError::Closed),
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(super) fn try_recv(&self, after: u64) -> Result<Arc<PublishedEvent>, RecvError> {
        match self.feed.lookup(self.workspace_id, after) {
            Some(Lookup::Ready(event)) => Ok(event),
            // Tests treat "nothing yet" as behind to keep assertions simple.
            Some(Lookup::Behind | Lookup::CaughtUp) => Err(RecvError::Behind),
            None => Err(RecvError::Closed),
        }
    }
}

impl Drop for FeedReceiver {
    fn drop(&mut self) {
        let removed = {
            let Ok(mut rings) = self.feed.rings.lock() else {
                return;
            };
            match rings.get_mut(&self.workspace_id) {
                // A live receiver always belongs to the current ring: a ring
                // is removed only once its last receiver has dropped.
                Some(ring) if Arc::ptr_eq(&ring.notify, &self.notify) => {
                    ring.subscribers = ring.subscribers.saturating_sub(1);
                    if ring.subscribers == 0 {
                        rings.remove(&self.workspace_id)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        // Freeing up to FEED_CAPACITY payloads happens outside the registry
        // lock so disconnecting cannot block another workspace.
        drop(removed);
    }
}

impl WorkspaceFeed {
    #[cfg(test)]
    pub(super) fn retained_workspaces(&self) -> usize {
        self.rings.lock().unwrap().len()
    }

    /// Publishes one committed batch in sequence order. Publishing with no
    /// live receivers is not an error: the events are already durable and a
    /// later subscriber catches up from SQLite.
    pub(super) fn publish(&self, events: Vec<Arc<PublishedEvent>>) {
        if events.is_empty() {
            return;
        }
        // Wake outside the registry lock so woken subscribers do not contend
        // with the publisher. A batch is one transaction and almost always
        // one workspace, so the common case allocates nothing.
        let mut wake: Option<Arc<Notify>> = None;
        let mut wake_more: Vec<Arc<Notify>> = Vec::new();
        {
            let Ok(mut rings) = self.rings.lock() else {
                return;
            };
            for event in events {
                let Some(ring) = rings.get_mut(&event.envelope.cursor.workspace_id) else {
                    continue;
                };
                ring.push(event);
                match &wake {
                    None => wake = Some(Arc::clone(&ring.notify)),
                    Some(first) if Arc::ptr_eq(first, &ring.notify) => {}
                    Some(_) => {
                        if !wake_more
                            .iter()
                            .any(|other| Arc::ptr_eq(other, &ring.notify))
                        {
                            wake_more.push(Arc::clone(&ring.notify));
                        }
                    }
                }
            }
        }
        if let Some(notify) = wake {
            notify.notify_waiters();
        }
        for notify in wake_more {
            notify.notify_waiters();
        }
    }

    /// Attaches from memory when the workspace ring covers `after`: the
    /// catch-up page comes from the ring and no store job runs. A ring exists
    /// only for a workspace the store validated for an earlier subscriber.
    pub(super) fn attach_warm(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        after: u64,
        limit: u16,
    ) -> Option<(FeedReceiver, Vec<Arc<PublishedEvent>>)> {
        let mut rings = self.rings.lock().ok()?;
        let ring = rings.get_mut(&workspace_id)?;
        if !ring.covers(after) {
            return None;
        }
        let page = ring.page(after, limit);
        ring.subscribers += 1;
        let receiver = FeedReceiver {
            feed: Arc::clone(self),
            workspace_id,
            notify: Arc::clone(&ring.notify),
        };
        Some((receiver, page))
    }

    /// Joins or creates the workspace ring from a store job that validated
    /// the workspace and read `page`. Nothing commits while a store job runs,
    /// so a short page's last sequence is the workspace tail at this moment
    /// and lets the next subscriber at that cursor attach warm.
    pub(super) fn attach_cold(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        page: &[Arc<PublishedEvent>],
        short: bool,
    ) -> Option<FeedReceiver> {
        let mut rings = self.rings.lock().ok()?;
        let ring = rings.entry(workspace_id).or_insert_with(Ring::new);
        if short && let Some(last) = page.last() {
            let sequence = last.sequence();
            ring.tail = Some(ring.tail.map_or(sequence, |tail| tail.max(sequence)));
        }
        ring.subscribers += 1;
        Some(FeedReceiver {
            feed: Arc::clone(self),
            workspace_id,
            notify: Arc::clone(&ring.notify),
        })
    }

    /// A catch-up page from memory, when the ring covers `after`.
    pub(super) fn warm_page(
        &self,
        workspace_id: WorkspaceId,
        after: u64,
        limit: u16,
    ) -> Option<Vec<Arc<PublishedEvent>>> {
        let rings = self.rings.lock().ok()?;
        let ring = rings.get(&workspace_id)?;
        ring.covers(after).then(|| ring.page(after, limit))
    }

    #[cfg(test)]
    pub(super) fn subscribe(self: &Arc<Self>, workspace_id: WorkspaceId) -> Option<FeedReceiver> {
        self.attach_cold(workspace_id, &[], false)
    }

    fn lookup(&self, workspace_id: WorkspaceId, after: u64) -> Option<Lookup> {
        let rings = self.rings.lock().ok()?;
        Some(rings.get(&workspace_id)?.lookup(after))
    }
}

#[cfg(test)]
mod tests {
    use qq_protocol::{EventCursor, RunActivity, RunId, SessionEvent, SessionId, StoreId};

    use super::*;

    fn published(sequence: u64) -> Arc<PublishedEvent> {
        let envelope = SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([1; 16]),
                workspace_id: WorkspaceId::from_bytes([7; 16]),
                sequence,
            },
            session_id: SessionId::from_bytes([2; 16]),
            run_id: None,
            caused_by: None,
            occurred_at_ms: 0,
            event: SessionEvent::RunActivityChanged {
                run_id: RunId::from_bytes([3; 16]),
                activity: RunActivity::WaitingForProvider,
            },
        };
        let json = Arc::from(serde_json::to_string(&envelope).expect("encodes"));
        Arc::new(PublishedEvent { envelope, json })
    }

    fn workspace() -> WorkspaceId {
        published(1).envelope.cursor.workspace_id
    }

    fn sequences(page: &[Arc<PublishedEvent>]) -> Vec<u64> {
        page.iter().map(|event| event.sequence()).collect()
    }

    #[test]
    fn staged_events_are_taken_as_one_batch_and_the_outbox_is_left_empty() {
        assert!(take_staged().is_empty());
        let event = published(1);
        stage(Arc::clone(&event));
        stage(event);
        assert_eq!(take_staged().len(), 2);
        assert!(take_staged().is_empty());
    }

    #[test]
    fn publishing_without_subscribers_retains_no_feed() {
        let feed = WorkspaceFeed::default();
        feed.publish(vec![published(1), published(2)]);
        assert_eq!(feed.retained_workspaces(), 0);
    }

    #[test]
    fn the_last_subscriber_releases_its_feed_and_buffered_events() {
        let feed = Arc::new(WorkspaceFeed::default());
        let event = published(1);
        let workspace_id = event.envelope.cursor.workspace_id;
        let first = feed.subscribe(workspace_id).unwrap();
        let second = feed.subscribe(workspace_id).unwrap();
        feed.publish(vec![event]);
        drop(first);
        assert_eq!(feed.retained_workspaces(), 1);
        assert_eq!(second.try_recv(0).unwrap().sequence(), 1);
        let unread = published(2);
        let buffered = Arc::downgrade(&unread);
        feed.publish(vec![unread]);
        assert!(buffered.upgrade().is_some());
        drop(second);
        assert_eq!(feed.retained_workspaces(), 0);
        assert!(buffered.upgrade().is_none());
    }

    #[test]
    fn final_drop_racing_a_new_subscriber_preserves_the_new_feed() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        for sequence in 1..=64 {
            let previous = feed.subscribe(workspace_id).unwrap();
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    drop(previous);
                });
                barrier.wait();
                let current = feed.subscribe(workspace_id).unwrap();
                feed.publish(vec![published(sequence)]);
                assert_eq!(current.try_recv(sequence - 1).unwrap().sequence(), sequence);
                drop(current);
            });
            assert_eq!(feed.retained_workspaces(), 0);
        }
    }

    #[tokio::test]
    async fn the_feed_delivers_in_order_and_a_late_subscriber_sees_nothing_earlier() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        feed.publish(vec![published(1)]);
        let receiver = feed.subscribe(workspace_id).expect("lock");
        feed.publish(vec![published(2), published(3)]);
        // A cold receiver's cursor comes from its store page; here it is 1.
        assert_eq!(receiver.recv(1).await.unwrap().sequence(), 2);
        assert_eq!(receiver.recv(2).await.unwrap().sequence(), 3);
        assert!(
            receiver.try_recv(3).is_err(),
            "nothing before the subscribe is replayed"
        );
    }

    #[test]
    fn a_warm_attach_serves_its_page_from_the_ring_without_a_store_read() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        assert!(
            feed.attach_warm(workspace_id, 0, 128).is_none(),
            "no ring: the store must validate the workspace"
        );
        let anchor = feed.subscribe(workspace_id).unwrap();
        assert!(
            feed.attach_warm(workspace_id, 0, 128).is_none(),
            "an empty ring with no known tail cannot vouch for a cursor"
        );
        feed.publish((1..=5).map(published).collect());
        let (warm, page) = feed.attach_warm(workspace_id, 2, 128).unwrap();
        assert_eq!(sequences(&page), [3, 4, 5]);
        assert_eq!(feed.retained_workspaces(), 1);
        let (caught_up, page) = feed.attach_warm(workspace_id, 5, 128).unwrap();
        assert!(page.is_empty());
        assert!(
            feed.attach_warm(workspace_id, 6, 128).is_none(),
            "a cursor past the tail is not covered"
        );
        let (_, limited) = feed.attach_warm(workspace_id, 0, 2).unwrap();
        assert_eq!(sequences(&limited), [1, 2]);
        drop((anchor, warm, caught_up));
        assert_eq!(feed.retained_workspaces(), 0);
    }

    #[test]
    fn a_short_cold_page_seeds_the_tail_so_the_next_subscriber_attaches_warm() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let page = vec![published(1), published(2)];
        let first = feed.attach_cold(workspace_id, &page, true).unwrap();
        let (second, warm_page) = feed
            .attach_warm(workspace_id, 2, 128)
            .expect("the tail is known");
        assert!(warm_page.is_empty());
        assert!(
            feed.attach_warm(workspace_id, 1, 128).is_none(),
            "events before the ring were never retained"
        );
        feed.publish(vec![published(3)]);
        assert_eq!(second.try_recv(2).unwrap().sequence(), 3);
        drop((first, second));
        assert_eq!(feed.retained_workspaces(), 0);
    }

    #[test]
    fn a_full_cold_page_does_not_claim_a_tail() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let page = vec![published(1), published(2)];
        let _first = feed.attach_cold(workspace_id, &page, false).unwrap();
        assert!(feed.attach_warm(workspace_id, 2, 128).is_none());
    }

    #[test]
    fn a_subscriber_behind_the_ring_is_sent_to_the_store() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let receiver = feed.subscribe(workspace_id).unwrap();
        let newest = FEED_CAPACITY as u64 + 8;
        feed.publish((1..=newest).map(published).collect());
        let (oldest, len) = {
            let rings = feed.rings.lock().unwrap();
            let ring = rings.get(&workspace_id).unwrap();
            (ring.events.front().unwrap().sequence(), ring.events.len())
        };
        assert!(len <= FEED_CAPACITY, "{len} retained");
        assert!(oldest > 8, "the first events were evicted");
        assert_eq!(receiver.try_recv(0).unwrap_err(), RecvError::Behind);
        assert_eq!(
            receiver.try_recv(oldest - 2).unwrap_err(),
            RecvError::Behind
        );
        assert_eq!(receiver.try_recv(oldest - 1).unwrap().sequence(), oldest);
        assert_eq!(receiver.try_recv(newest - 1).unwrap().sequence(), newest);
        assert!(feed.warm_page(workspace_id, 0, 128).is_none());
        assert_eq!(
            sequences(&feed.warm_page(workspace_id, oldest - 1, 4).unwrap()),
            [oldest, oldest + 1, oldest + 2, oldest + 3]
        );
    }

    #[test]
    fn large_payloads_are_bounded_by_bytes_not_only_by_count() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let receiver = feed.subscribe(workspace_id).unwrap();
        // ~64 KiB per event: far fewer than FEED_CAPACITY fit in the byte bound.
        let big = |sequence: u64| {
            let template = published(sequence);
            let padding = "x".repeat(64 * 1024);
            Arc::new(PublishedEvent {
                envelope: template.envelope.clone(),
                json: Arc::from(format!("{}{padding}", template.json)),
            })
        };
        feed.publish((1..=16).map(big).collect());
        let retained = feed.warm_page(workspace_id, 0, 128);
        assert!(retained.is_none(), "the oldest events were evicted");
        let (first_retained, _) = {
            let rings = feed.rings.lock().unwrap();
            let ring = rings.get(&workspace_id).unwrap();
            assert!(ring.retained_bytes <= FEED_RETAINED_BYTES + 64 * 1024 + 512);
            assert!(ring.events.len() < 16 && !ring.events.is_empty());
            (ring.events.front().unwrap().sequence(), ())
        };
        assert_eq!(
            receiver.try_recv(first_retained - 2).unwrap_err(),
            RecvError::Behind
        );
        assert_eq!(receiver.try_recv(15).unwrap().sequence(), 16);
    }

    #[test]
    fn a_non_contiguous_publish_discards_the_ring_rather_than_serve_a_hole() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let receiver = feed.subscribe(workspace_id).unwrap();
        feed.publish(vec![published(1), published(2)]);
        feed.publish(vec![published(5)]);
        assert_eq!(receiver.try_recv(2).unwrap_err(), RecvError::Behind);
        assert_eq!(receiver.try_recv(4).unwrap().sequence(), 5);
        assert!(feed.warm_page(workspace_id, 1, 128).is_none());
    }

    #[tokio::test]
    async fn a_waiting_receiver_is_woken_by_a_publish_after_it_registered() {
        let feed = Arc::new(WorkspaceFeed::default());
        let workspace_id = workspace();
        let receiver = feed.subscribe(workspace_id).unwrap();
        let wait = tokio::spawn({
            let feed = Arc::clone(&feed);
            async move {
                let receiver = feed.subscribe(workspace_id).unwrap();
                receiver.recv(0).await.map(|event| event.sequence())
            }
        });
        tokio::task::yield_now().await;
        feed.publish(vec![published(1)]);
        assert_eq!(wait.await.unwrap(), Ok(1));
        assert_eq!(receiver.try_recv(0).unwrap().sequence(), 1);
    }

    #[test]
    fn into_envelope_moves_when_unique_and_clones_when_shared() {
        assert_eq!(
            PublishedEvent::into_envelope(published(9)).cursor.sequence,
            9
        );
        let shared = published(10);
        let other = Arc::clone(&shared);
        assert_eq!(PublishedEvent::into_envelope(shared).cursor.sequence, 10);
        assert_eq!(other.envelope.cursor.sequence, 10);
    }
}
