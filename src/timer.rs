#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::Cell, cell::RefCell, time::Duration, time::Instant};

use ntex_util::time::{Seconds, now, sleep};
use ntex_util::{HashMap, HashSet, spawn};

use crate::stream::StreamRef;

const CAP: usize = 4;
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
                    let stop = TIMER.with(|timer| {
                        let current = timer.current.get();
                        timer.current.set(current + 1);

                        // notify io dispatcher
                        let mut inner = timer.storage.borrow_mut();
                        while let Some(key) = inner.notifications.keys().next() {
                            let key = *key;
                            if key <= current {
                                let mut items = inner.notifications.remove(&key).unwrap();
                                for io in items.drain() {
                                    if let Some(hnd) = inner.streams.remove(&io) {
                                        if hnd == key {
                                            io.capacity_timeout();
                                        } else {
                                            inner.streams.insert(io, hnd);
                                        }
                                    }
                                }
                                if inner.cache.len() <= CAP {
                                    inner.cache.push_back(items);
                                }
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
        TIMER.with(|timer| {
            timer.running.set(false);
            timer.storage.borrow_mut().notifications.clear();
        });
    }
}
