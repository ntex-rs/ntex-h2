#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::Cell, cell::RefCell, mem, rc::Weak, time::Duration, time::Instant};

use ntex_util::time::{Seconds, now, sleep};
use ntex_util::{HashMap, HashSet, spawn};

use crate::connection::{Connection, ConnectionState};
use crate::stream::StreamRef;

const CAP: usize = 4;
/// Larger drained buckets are released instead of being kept for reuse.
const MAX_CACHED_CAPACITY: usize = 128;
const SEC: Duration = Duration::from_secs(1);

thread_local! {
    static TIMER: Inner = Inner {
        running: Cell::new(false),
        base: Cell::new(Instant::now()),
        current: Cell::new(0),
        next_id: Cell::new(0),
        storage: RefCell::new(InnerMut {
            cache: VecDeque::with_capacity(CAP),
            streams: HashMap::default(),
            notifications: BTreeMap::default(),
            connections: HashMap::default(),
            deadlines: BTreeMap::default(),
        })
    }
}

struct Inner {
    running: Cell<bool>,
    base: Cell<Instant>,
    current: Cell<u32>,
    next_id: Cell<u64>,
    storage: RefCell<InnerMut>,
}

struct InnerMut {
    cache: VecDeque<HashSet<StreamRef>>,
    streams: HashMap<StreamRef, u32>,
    notifications: BTreeMap<u32, HashSet<StreamRef>>,
    // connection keep-alive and settings deadlines, keyed by connection id
    connections: HashMap<u64, ConnectionTimer>,
    // connection ids by deadline, stale ids are skipped on expiry
    deadlines: BTreeMap<u32, Vec<u64>>,
}

struct ConnectionTimer {
    con: Weak<ConnectionState>,
    ping_timeout: u32,
    settings: Option<u32>,
    ping: Option<u32>,
}

impl ConnectionTimer {
    fn is_empty(&self) -> bool {
        self.settings.is_none() && self.ping.is_none()
    }
}

impl InnerMut {
    fn unregister(&mut self, io: &StreamRef) {
        if let Some(id) = self.streams.remove(io)
            && let Some(streams) = self.notifications.get_mut(&id)
        {
            streams.remove(io);
        }
    }

    fn add_deadline(&mut self, id: u64, hnd: u32) {
        self.deadlines.entry(hnd).or_default().push(id);
    }

    fn is_empty(&self) -> bool {
        self.notifications.is_empty() && self.deadlines.is_empty()
    }

    /// Keeps a drained bucket for reuse, burst-sized buckets are released.
    fn recycle(&mut self, items: HashSet<StreamRef>) {
        debug_assert!(items.is_empty());
        if self.cache.len() < CAP && items.capacity() <= MAX_CACHED_CAPACITY {
            self.cache.push_back(items);
        }
    }
}

pub(crate) fn unregister(io: &StreamRef) {
    TIMER.with(|timer| {
        timer.storage.borrow_mut().unregister(io);
    });
}

pub(crate) fn register(timeout: Seconds, io: &StreamRef) {
    TIMER.with(|timer| {
        timer.update_current();

        // insert handle
        let hnd = timer.current.get() + u32::from(timeout.0);
        let mut inner = timer.storage.borrow_mut();

        // insert key
        if let Some(item) = inner.notifications.range_mut(hnd..=hnd).next() {
            item.1.insert(io.clone());
        } else {
            let mut items = inner.cache.pop_front().unwrap_or_default();
            items.insert(io.clone());
            inner.notifications.insert(hnd, items);
        }

        if let Some(key) = inner.streams.get(io).copied()
            && key != hnd
            && let Some(items) = inner.notifications.get_mut(&key)
        {
            items.remove(io);
        }
        inner.streams.insert(io.clone(), hnd);
        drop(inner);

        timer.start();
    });
}

/// Registers connection settings and keep-alive deadlines, a zero timeout
/// disables the corresponding deadline.
///
/// Returns the connection timer id, or `0` if both deadlines are disabled.
pub(crate) fn register_connection(con: Weak<ConnectionState>, settings: Seconds, ping: Seconds) -> u64 {
    if !settings.non_zero() && !ping.non_zero() {
        return 0;
    }

    TIMER.with(|timer| {
        timer.update_current();

        let id = timer.next_id.get() + 1;
        timer.next_id.set(id);

        let current = timer.current.get();
        let deadline = |timeout: Seconds| timeout.non_zero().then(|| current + u32::from(timeout.0));
        let entry = ConnectionTimer {
            con,
            ping_timeout: u32::from(ping.0),
            settings: deadline(settings),
            ping: deadline(ping),
        };

        let mut inner = timer.storage.borrow_mut();
        for hnd in [entry.settings, entry.ping].into_iter().flatten() {
            inner.add_deadline(id, hnd);
        }
        inner.connections.insert(id, entry);
        drop(inner);

        timer.start();
        id
    })
}

/// Removes connection deadlines and releases the connection reference.
pub(crate) fn unregister_connection(id: u64) {
    if id != 0 {
        // thread local storage can be destroyed already on thread shutdown,
        // a busy storage drops the stale entry on its next deadline
        let _ = TIMER.try_with(|timer| {
            if let Ok(mut inner) = timer.storage.try_borrow_mut() {
                inner.connections.remove(&id)
            } else {
                None
            }
        });
    }
}

/// Removes the settings deadline after the local settings are acknowledged.
pub(crate) fn settings_acked(id: u64) {
    if id != 0 {
        TIMER.with(|timer| {
            let mut inner = timer.storage.borrow_mut();
            if let Some(entry) = inner.connections.get_mut(&id) {
                entry.settings = None;
                if entry.is_empty() {
                    inner.connections.remove(&id);
                }
            }
        });
    }
}

impl Inner {
    /// Sets up the current tick if the timer driver does not run.
    fn update_current(&self) {
        if !self.running.get() {
            let current = (now() - self.base.get()).as_secs() as u32;
            self.current.set(current);
            log::debug!("Timer driver does not run, current: {current}");
        }
    }

    fn start(&self) {
        if !self.running.get() {
            self.running.set(true);
            spawn(driver());
        }
    }

    /// Collects expired stream and connection deadlines up to `current`.
    fn expire(
        &self,
        current: u32,
        streams: &mut Vec<StreamRef>,
        connections: &mut Vec<(u64, Weak<ConnectionState>, bool, bool)>,
    ) {
        let mut inner = self.storage.borrow_mut();
        while let Some(key) = inner.notifications.keys().next() {
            let key = *key;
            if key <= current {
                let mut items = inner.notifications.remove(&key).unwrap();
                for io in items.drain() {
                    if let Some(hnd) = inner.streams.remove(&io) {
                        if hnd == key {
                            streams.push(io);
                        } else {
                            inner.streams.insert(io, hnd);
                        }
                    }
                }
                inner.recycle(items);
            } else {
                break;
            }
        }

        while let Some(entry) = inner.deadlines.first_entry() {
            if *entry.key() > current {
                break;
            }
            let (key, ids) = entry.remove_entry();
            for id in ids {
                if let Some(entry) = inner.connections.get_mut(&id) {
                    let settings = entry.settings == Some(key);
                    let ping = entry.ping == Some(key);
                    if settings {
                        entry.settings = None;
                    }
                    if ping {
                        entry.ping = None;
                    }
                    if settings || ping {
                        connections.push((id, entry.con.clone(), settings, ping));
                    }
                }
            }
        }
    }

    /// Schedules the next keep-alive ping or drops the finished connection entry.
    fn reschedule(&self, id: u64, current: u32, ping: bool) {
        let mut inner = self.storage.borrow_mut();
        if let Some(entry) = inner.connections.get_mut(&id) {
            if ping {
                let hnd = current + entry.ping_timeout;
                entry.ping = Some(hnd);
                inner.add_deadline(id, hnd);
            } else if entry.is_empty() {
                inner.connections.remove(&id);
            }
        }
    }
}

async fn driver() {
    let guard = TimerGuard;
    let mut streams = Vec::new();
    let mut connections = Vec::new();
    loop {
        sleep(SEC).await;
        let current = TIMER.with(|timer| {
            let current = timer.current.get();
            timer.current.set(current + 1);
            timer.expire(current, &mut streams, &mut connections);
            current
        });

        // notify streams and connections outside of the storage borrow, a timeout
        // can fail other streams and unregister their timers
        for io in streams.drain(..) {
            io.capacity_timeout();
        }
        for (id, con, settings, ping) in connections.drain(..) {
            let ping = Connection::timer_expired(&con, settings, ping);
            drop(con);
            TIMER.with(|timer| timer.reschedule(id, current, ping));
        }

        // new registrations during notification keep the driver running
        let stop = TIMER.with(|timer| {
            if timer.storage.borrow().is_empty() {
                timer.running.set(false);
                true
            } else {
                false
            }
        });
        if stop {
            break;
        }
    }
    drop(guard);
}

struct TimerGuard;

impl Drop for TimerGuard {
    fn drop(&mut self) {
        // thread local storage can be destroyed already on thread shutdown
        let (streams, _notifications, _connections) = TIMER
            .try_with(|timer| {
                timer.running.set(false);
                let mut inner = timer.storage.borrow_mut();
                inner.deadlines.clear();
                (
                    mem::take(&mut inner.streams),
                    mem::take(&mut inner.notifications),
                    mem::take(&mut inner.connections),
                )
            })
            .unwrap_or_default();

        // release stream references outside of the storage borrow
        for io in streams.into_keys() {
            io.stop_capacity_timer();
        }
    }
}

#[cfg(test)]
pub(crate) fn cancel() {
    drop(TimerGuard);
}

#[cfg(test)]
pub(crate) fn is_registered(io: &StreamRef) -> bool {
    TIMER.with(|timer| timer.storage.borrow().streams.contains_key(io))
}

#[cfg(test)]
pub(crate) fn is_running() -> bool {
    TIMER.with(|timer| timer.running.get())
}

/// Returns `(settings, ping)` deadline presence of a registered connection.
#[cfg(test)]
pub(crate) fn connection_deadlines(id: u64) -> Option<(bool, bool)> {
    TIMER.with(|timer| {
        let inner = timer.storage.borrow();
        inner
            .connections
            .get(&id)
            .map(|e| (e.settings.is_some(), e.ping.is_some()))
    })
}

#[cfg(test)]
mod tests {
    use ntex_io::{Io, testing::IoTest};
    use ntex_service::cfg::SharedCfg;
    use ntex_util::{channel::pool, time::Millis};

    use super::*;
    use crate::{ServiceConfig, codec::Codec, connection::Connection, frame::StreamId};

    fn cached() -> Vec<usize> {
        TIMER.with(|timer| {
            let inner = timer.storage.borrow();
            inner.cache.iter().map(HashSet::capacity).collect()
        })
    }

    /// STATEMENT: The bucket cache holds at most `CAP` drained buckets, none larger than
    /// `MAX_CACHED_CAPACITY`.
    ///
    /// HOW:
    /// - Recycle `CAP + 2` small buckets and verify only `CAP` are cached.
    /// - Recycle an oversized bucket into an empty cache and verify it is released, while a
    ///   bucket at the capacity bound is kept.
    #[test]
    fn recycle_bounds_cache() {
        let mut inner = InnerMut {
            cache: VecDeque::new(),
            streams: HashMap::default(),
            notifications: BTreeMap::default(),
            connections: HashMap::default(),
            deadlines: BTreeMap::default(),
        };
        for _ in 0..CAP + 2 {
            inner.recycle(HashSet::default());
        }
        assert_eq!(inner.cache.len(), CAP);

        inner.cache.clear();
        let mut items = HashSet::default();
        items.reserve(MAX_CACHED_CAPACITY * 8);
        inner.recycle(items);
        assert_eq!(inner.cache.len(), 0);

        let mut items = HashSet::default();
        items.reserve(MAX_CACHED_CAPACITY / 2);
        assert!(items.capacity() <= MAX_CACHED_CAPACITY);
        inner.recycle(items);
        assert_eq!(inner.cache.len(), 1);
    }

    /// STATEMENT: A burst of capacity timers does not leave its bucket allocation cached after
    /// the deadline passes, while a small bucket is kept for reuse.
    ///
    /// HOW:
    /// - Register 1000 streams for a one-second deadline and two streams for a two-second
    ///   deadline, then unregister them all, as when capacity recovers.
    /// - Wait until both deadlines expire and the timer task stops.
    /// - Verify the cache holds only the small bucket, within `MAX_CACHED_CAPACITY`.
    #[ntex::test]
    async fn burst_bucket_is_released() {
        // connection deadlines would keep the shared driver running
        let cfg = SharedCfg::new("SRV")
            .add(
                ServiceConfig::new()
                    .set_ping_timeout(Seconds::ZERO)
                    .set_settings_timeout(Seconds::ZERO),
            )
            .build();
        let (wire, _peer) = IoTest::create();
        let io = Io::new(wire, cfg.clone());
        let con = Connection::new(
            true,
            io.get_ref(),
            Codec::default(),
            cfg.get(),
            false,
            false,
            pool::new(),
        );
        let stream = |id: u32| StreamRef::new(StreamId::from(id * 2 + 1), true, con.clone());

        let burst: Vec<_> = (0..1000).map(stream).collect();
        let small: Vec<_> = (1000..1002).map(stream).collect();
        for io in &burst {
            register(Seconds(1), io);
        }
        for io in &small {
            register(Seconds(2), io);
        }
        for io in burst.iter().chain(&small) {
            unregister(io);
        }

        sleep(Millis(3500)).await;
        assert!(!TIMER.with(|timer| timer.running.get()));
        let cache = cached();
        assert_eq!(cache.len(), 1);
        assert!(cache[0] <= MAX_CACHED_CAPACITY);
    }
}
