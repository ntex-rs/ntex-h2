#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::Cell, cell::RefCell, mem, time::Duration, time::Instant};

use ntex_util::time::{Seconds, now, sleep};
use ntex_util::{HashMap, HashSet, spawn};

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
        storage: RefCell::new(InnerMut {
            cache: VecDeque::with_capacity(CAP),
            streams: HashMap::default(),
            notifications: BTreeMap::default(),
        })
    }
}

struct Inner {
    running: Cell<bool>,
    base: Cell<Instant>,
    current: Cell<u32>,
    storage: RefCell<InnerMut>,
}

struct InnerMut {
    cache: VecDeque<HashSet<StreamRef>>,
    streams: HashMap<StreamRef, u32>,
    notifications: BTreeMap<u32, HashSet<StreamRef>>,
}

impl InnerMut {
    fn unregister(&mut self, io: &StreamRef) {
        if let Some(id) = self.streams.remove(io)
            && let Some(streams) = self.notifications.get_mut(&id)
        {
            streams.remove(io);
        }
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
        // setup current delta
        if !timer.running.get() {
            let current = (now() - timer.base.get()).as_secs() as u32;
            timer.current.set(current);
            log::debug!("{}: Timer driver does not run, current: {}", io.tag(), current);
        }

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

        // start timer task
        if !timer.running.get() {
            timer.running.set(true);

            spawn(async move {
                let guard = TimerGuard;
                loop {
                    sleep(SEC).await;
                    let mut expired = Vec::new();
                    let stop = TIMER.with(|timer| {
                        let current = timer.current.get();
                        timer.current.set(current + 1);

                        // collect expired streams
                        let mut inner = timer.storage.borrow_mut();
                        while let Some(key) = inner.notifications.keys().next() {
                            let key = *key;
                            if key <= current {
                                let mut items = inner.notifications.remove(&key).unwrap();
                                for io in items.drain() {
                                    if let Some(hnd) = inner.streams.remove(&io) {
                                        if hnd == key {
                                            expired.push(io);
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

                        // new tick
                        if inner.notifications.is_empty() {
                            timer.running.set(false);
                            true
                        } else {
                            false
                        }
                    });

                    // notify streams outside of the storage borrow, a timeout
                    // can fail other streams and unregister their timers
                    for io in expired {
                        io.capacity_timeout();
                    }

                    if stop {
                        break;
                    }
                }
                drop(guard);
            });
        }
    });
}

struct TimerGuard;

impl Drop for TimerGuard {
    fn drop(&mut self) {
        // thread local storage can be destroyed already on thread shutdown
        let (streams, _notifications) = TIMER
            .try_with(|timer| {
                timer.running.set(false);
                let mut inner = timer.storage.borrow_mut();
                (mem::take(&mut inner.streams), mem::take(&mut inner.notifications))
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
        let cfg = SharedCfg::new("SRV").add(ServiceConfig::new()).build();
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
