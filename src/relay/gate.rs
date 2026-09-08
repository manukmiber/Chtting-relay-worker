//! Admission control: how many requests run at once, and what happens to the
//! rest.
//!
//! A phone has a fixed amount of CPU and a fixed amount of RAM. Past some
//! number of simultaneous generations it does not go faster, it goes slower
//! for everyone — so the relay picks a number and holds to it.
//!
//! Requests over that number wait in line rather than being turned away:
//! a caller who waits 300 ms for a slot and then gets an answer is better
//! served than one who gets a 503 and retries into the same wall. The line
//! itself is bounded, though, and waiting in it has a deadline. Past either,
//! the honest answer is a 503 with `Retry-After`, because a queue nobody
//! reaches the front of is worse than no queue at all.
//!
//! The line lives in memory, not on disk. Every request in it is already an
//! open HTTP connection with a caller waiting on the other end; persisting it
//! would add disk latency to the very path that is under pressure, and buy
//! nothing, since the connection dies with the process either way.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Held for as long as a request occupies a slot; releases it when dropped.
pub struct Ticket {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicUsize>,
    /// How long this request waited before it got the slot.
    pub queued_ms: f64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What happened when a request asked for a slot.
pub enum Admission {
    Admitted(Ticket),
    /// The line is already as long as it is allowed to get.
    QueueFull {
        waiting: usize,
        capacity: usize,
    },
    /// Waited its turn and ran out of time.
    TimedOut {
        waited_ms: f64,
    },
}

/// A resizable concurrency limit with a bounded wait queue in front of it.
pub struct Gate {
    sem: Arc<Semaphore>,
    /// Permits the semaphore actually holds. May lag behind the configured
    /// limit while a shrink waits for in-flight requests to give slots back.
    effective: AtomicUsize,
    in_flight: Arc<AtomicUsize>,
    waiting: AtomicUsize,
    admitted_immediately: AtomicU64,
    admitted_after_wait: AtomicU64,
    refused_queue_full: AtomicU64,
    refused_timeout: AtomicU64,
    waited_ms_total: AtomicU64,
    peak_waiting: AtomicUsize,
}

impl Gate {
    pub fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            sem: Arc::new(Semaphore::new(limit)),
            effective: AtomicUsize::new(limit),
            in_flight: Arc::new(AtomicUsize::new(0)),
            waiting: AtomicUsize::new(0),
            admitted_immediately: AtomicU64::new(0),
            admitted_after_wait: AtomicU64::new(0),
            refused_queue_full: AtomicU64::new(0),
            refused_timeout: AtomicU64::new(0),
            waited_ms_total: AtomicU64::new(0),
            peak_waiting: AtomicUsize::new(0),
        }
    }

    /// Match the semaphore to a concurrency limit that may have changed.
    ///
    /// Growing is immediate. Shrinking can only take back slots that are free
    /// right now — a request already running keeps the one it holds — so a
    /// shrink finishes over the next few calls as those requests complete.
    /// Called on the request path, where the common case is one atomic load.
    pub fn resize(&self, limit: usize) {
        let target = limit.max(1);
        let current = self.effective.load(Ordering::Relaxed);
        if target == current {
            return;
        }
        if target > current {
            self.sem.add_permits(target - current);
            self.effective.store(target, Ordering::Relaxed);
        } else {
            let removed = self.sem.forget_permits(current - target);
            self.effective
                .store(current.saturating_sub(removed), Ordering::Relaxed);
        }
    }

    /// Ask for a slot, waiting in line if one is not free.
    pub async fn admit(&self, capacity: usize, wait: Duration) -> Admission {
        if let Ok(permit) = self.sem.clone().try_acquire_owned() {
            self.admitted_immediately.fetch_add(1, Ordering::Relaxed);
            self.in_flight.fetch_add(1, Ordering::Relaxed);
            return Admission::Admitted(Ticket {
                _permit: permit,
                in_flight: self.in_flight.clone(),
                queued_ms: 0.0,
            });
        }

        // Reserve a place in line before awaiting, so the bound holds even
        // when a hundred callers arrive in the same millisecond.
        let place = self.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        if place > capacity {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            self.refused_queue_full.fetch_add(1, Ordering::Relaxed);
            return Admission::QueueFull {
                waiting: place - 1,
                capacity,
            };
        }
        self.peak_waiting.fetch_max(place, Ordering::Relaxed);

        let started = Instant::now();
        // Tokio's semaphore hands out permits in the order they were asked
        // for, so this really is a queue: the caller who has waited longest
        // gets the next slot, and nobody starves behind a burst of newcomers.
        let outcome = tokio::time::timeout(wait, self.sem.clone().acquire_owned()).await;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        let waited_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.waited_ms_total
            .fetch_add(waited_ms as u64, Ordering::Relaxed);

        match outcome {
            Ok(Ok(permit)) => {
                self.admitted_after_wait.fetch_add(1, Ordering::Relaxed);
                self.in_flight.fetch_add(1, Ordering::Relaxed);
                Admission::Admitted(Ticket {
                    _permit: permit,
                    in_flight: self.in_flight.clone(),
                    queued_ms: waited_ms,
                })
            }
            // Timed out, or the semaphore was closed during shutdown. Either
            // way there is no slot and the caller has waited long enough.
            _ => {
                self.refused_timeout.fetch_add(1, Ordering::Relaxed);
                Admission::TimedOut { waited_ms }
            }
        }
    }

    pub fn snapshot(&self) -> GateStats {
        let after_wait = self.admitted_after_wait.load(Ordering::Relaxed);
        GateStats {
            limit: self.effective.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            peak_waiting: self.peak_waiting.load(Ordering::Relaxed),
            admitted_immediately: self.admitted_immediately.load(Ordering::Relaxed),
            admitted_after_wait: after_wait,
            refused_queue_full: self.refused_queue_full.load(Ordering::Relaxed),
            refused_timeout: self.refused_timeout.load(Ordering::Relaxed),
            avg_wait_ms: if after_wait == 0 {
                0.0
            } else {
                self.waited_ms_total.load(Ordering::Relaxed) as f64 / after_wait as f64
            },
        }
    }
}

/// What the dashboard shows about the queue.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStats {
    pub limit: usize,
    pub in_flight: usize,
    pub waiting: usize,
    pub peak_waiting: usize,
    pub admitted_immediately: u64,
    pub admitted_after_wait: u64,
    pub refused_queue_full: u64,
    pub refused_timeout: u64,
    pub avg_wait_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_WAIT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn requests_past_the_limit_wait_rather_than_being_refused() {
        let gate = Arc::new(Gate::new(1));
        let held = match gate.admit(4, NO_WAIT).await {
            Admission::Admitted(t) => t,
            _ => panic!("the first request must be admitted"),
        };

        let second = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.admit(4, Duration::from_secs(5)).await })
        };

        // Give it a moment to actually be waiting, then let it through.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(gate.snapshot().waiting, 1);
        drop(held);

        match second.await.expect("the waiter should not panic") {
            Admission::Admitted(t) => assert!(
                t.queued_ms > 0.0,
                "a request that waited should say how long"
            ),
            _ => panic!("the queued request should have been let through"),
        }
    }

    #[tokio::test]
    async fn a_full_queue_is_refused_immediately() {
        let gate = Arc::new(Gate::new(1));
        let _held = match gate.admit(1, NO_WAIT).await {
            Admission::Admitted(t) => t,
            _ => panic!("the first request must be admitted"),
        };

        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.admit(1, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        // One slot, one place in line, both taken: this one is refused without
        // waiting at all.
        let started = Instant::now();
        match gate.admit(1, Duration::from_secs(5)).await {
            Admission::QueueFull { capacity, .. } => assert_eq!(capacity, 1),
            _ => panic!("a full queue must refuse rather than wait"),
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the refusal must be immediate"
        );
        waiter.abort();
    }

    #[tokio::test]
    async fn waiting_too_long_gives_up() {
        let gate = Gate::new(1);
        let _held = match gate.admit(4, NO_WAIT).await {
            Admission::Admitted(t) => t,
            _ => panic!("the first request must be admitted"),
        };
        match gate.admit(4, Duration::from_millis(40)).await {
            Admission::TimedOut { waited_ms } => assert!(waited_ms >= 30.0),
            _ => panic!("it should have run out of time"),
        }
        assert_eq!(gate.snapshot().refused_timeout, 1);
    }

    #[tokio::test]
    async fn raising_the_limit_takes_effect_immediately() {
        let gate = Gate::new(1);
        let _first = gate.admit(0, NO_WAIT).await;
        // Nothing free and no room to wait.
        assert!(matches!(
            gate.admit(0, NO_WAIT).await,
            Admission::QueueFull { .. }
        ));

        gate.resize(4);
        assert!(matches!(
            gate.admit(0, NO_WAIT).await,
            Admission::Admitted(_)
        ));
        assert_eq!(gate.snapshot().limit, 4);
    }

    #[tokio::test]
    async fn lowering_the_limit_never_takes_a_slot_from_a_running_request() {
        let gate = Gate::new(4);
        let held: Vec<_> = {
            let mut out = Vec::new();
            for _ in 0..3 {
                match gate.admit(0, NO_WAIT).await {
                    Admission::Admitted(t) => out.push(t),
                    _ => panic!("all three should fit under a limit of four"),
                }
            }
            out
        };

        // Only the one free slot can be taken back right now.
        gate.resize(1);
        assert_eq!(gate.snapshot().limit, 3);
        assert_eq!(gate.snapshot().in_flight, 3);

        // As they finish, the rest of the shrink lands.
        drop(held);
        gate.resize(1);
        assert_eq!(gate.snapshot().limit, 1);
    }
}
