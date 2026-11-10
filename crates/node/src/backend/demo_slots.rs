//! Seats of a demonstration node: how many people use it at once, and for how
//! long.
//!
//! With `DEMO_MODE=1` a signed-in person does not reach the user surface just
//! for having signed in: they take one of `DEMO_MAX_SESSIONS` seats (default
//! 10) for `DEMO_SESSION_TIME` minutes (default 20). Whoever arrives while
//! every seat is taken waits in a first-come queue and sees their position and
//! an estimate of when a seat frees. Signing out frees the seat at once; a
//! seat whose time ran out is taken back and its session revoked.
//!
//! The state lives in memory only, keyed by the browser session id: a restart
//! of the node empties the seats and the queue, like it ends every session.
//! A queued person keeps their place by polling `GET /api/ui/demo/session`;
//! one who stops polling for `QUEUE_STALE` leaves the queue, so a closed tab
//! does not hold everyone behind it.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A queued person that has not polled for this long leaves the queue.
const QUEUE_STALE: Duration = Duration::from_secs(45);
const DEFAULT_MAX_SESSIONS: usize = 10;
const DEFAULT_SESSION_MINUTES: u64 = 20;

/// `DEMO_MAX_SESSIONS` and `DEMO_SESSION_TIME` (minutes), each also under the
/// brand prefix, read once.
pub fn limits() -> (usize, Duration) {
    static LIMITS: OnceLock<(usize, Duration)> = OnceLock::new();
    *LIMITS.get_or_init(|| {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .or_else(|| crate::brand::env(name).ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
        };
        let max = read("DEMO_MAX_SESSIONS")
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_SESSIONS);
        let minutes = read("DEMO_SESSION_TIME").unwrap_or(DEFAULT_SESSION_MINUTES);
        (max, Duration::from_secs(minutes * 60))
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum SeatStatus {
    /// The person holds a seat until `expires_at`.
    Active { expires_at: Instant },
    /// Waiting: `position` is 1-based, `waiting` the queue's length and
    /// `eta` the estimate until a seat frees for this position.
    Queued {
        position: usize,
        waiting: usize,
        eta: Duration,
    },
    /// The seat's time ran out on this request: the session must end.
    Expired,
}

impl SeatStatus {
    pub fn to_json(&self, now: Instant) -> Value {
        let (max, ttl) = limits();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match self {
            SeatStatus::Active { expires_at } => {
                let remaining = expires_at.saturating_duration_since(now).as_secs();
                json!({
                    "state": "active",
                    "remaining_secs": remaining,
                    "expires_at": unix_now + remaining,
                    "session_secs": ttl.as_secs(),
                    "max_sessions": max,
                })
            }
            SeatStatus::Queued { position, waiting, eta } => json!({
                "state": "queued",
                "position": position,
                "waiting": waiting,
                "eta_secs": eta.as_secs(),
                "session_secs": ttl.as_secs(),
                "max_sessions": max,
            }),
            SeatStatus::Expired => json!({ "state": "expired" }),
        }
    }
}

struct Seat {
    user_id: String,
    expires_at: Instant,
}

struct Waiting {
    session: String,
    user_id: String,
    last_seen: Instant,
}

/// Seats and queue. Pure state: the clock and the limits come in, so tests
/// drive it without waiting.
pub struct Seats {
    max: usize,
    ttl: Duration,
    active: HashMap<String, Seat>,
    queue: VecDeque<Waiting>,
}

impl Seats {
    pub fn new(max: usize, ttl: Duration) -> Self {
        Self {
            max: max.max(1),
            ttl,
            active: HashMap::new(),
            queue: VecDeque::new(),
        }
    }

    /// Takes back expired seats and stale queue places; returns the sessions
    /// whose seat expired, for the caller to revoke.
    fn sweep(&mut self, now: Instant) -> Vec<(String, String)> {
        let expired = self
            .active
            .iter()
            .filter(|(_, seat)| seat.expires_at <= now)
            .map(|(session, seat)| (session.clone(), seat.user_id.clone()))
            .collect::<Vec<_>>();
        for (session, _) in &expired {
            self.active.remove(session);
        }
        self.queue
            .retain(|waiting| now.duration_since(waiting.last_seen) < QUEUE_STALE);
        expired
    }

    fn promote(&mut self, now: Instant) {
        while self.active.len() < self.max {
            let Some(next) = self.queue.pop_front() else {
                break;
            };
            self.active.insert(
                next.session,
                Seat {
                    user_id: next.user_id,
                    expires_at: now + self.ttl,
                },
            );
        }
    }

    fn eta(&self, position: usize, now: Instant) -> Duration {
        if self.active.len() < self.max {
            return Duration::ZERO;
        }
        let mut remaining = self
            .active
            .values()
            .map(|seat| seat.expires_at.saturating_duration_since(now))
            .collect::<Vec<_>>();
        remaining.sort();
        let index = position.saturating_sub(1);
        let round = (index / self.max) as u32;
        remaining[index % self.max] + self.ttl * round
    }

    /// Where this session stands, joining the queue (or taking a free seat)
    /// on its first call. `expired` lists every session whose seat ran out in
    /// this sweep, this one included.
    pub fn status(
        &mut self,
        session: &str,
        user_id: &str,
        now: Instant,
    ) -> (SeatStatus, Vec<(String, String)>) {
        let expired = self.sweep(now);
        if expired.iter().any(|(id, _)| id == session) {
            self.promote(now);
            return (SeatStatus::Expired, expired);
        }
        if let Some(seat) = self.active.get(session) {
            return (SeatStatus::Active { expires_at: seat.expires_at }, expired);
        }
        // The same person signing in again (another browser) moves their
        // seat to the new session and keeps its clock: one person, one seat.
        if let Some(old) = self
            .active
            .iter()
            .find(|(_, seat)| seat.user_id == user_id)
            .map(|(id, _)| id.clone())
        {
            if let Some(seat) = self.active.remove(&old) {
                let expires_at = seat.expires_at;
                self.active.insert(session.to_owned(), seat);
                return (SeatStatus::Active { expires_at }, expired);
            }
        }
        match self.queue.iter_mut().find(|w| w.session == session) {
            Some(waiting) => waiting.last_seen = now,
            None => {
                self.queue.retain(|w| w.user_id != user_id);
                self.queue.push_back(Waiting {
                    session: session.to_owned(),
                    user_id: user_id.to_owned(),
                    last_seen: now,
                });
            }
        }
        self.promote(now);
        if let Some(seat) = self.active.get(session) {
            return (SeatStatus::Active { expires_at: seat.expires_at }, expired);
        }
        let position = self
            .queue
            .iter()
            .position(|w| w.session == session)
            .map(|index| index + 1)
            .unwrap_or(1);
        let status = SeatStatus::Queued {
            position,
            waiting: self.queue.len(),
            eta: self.eta(position, now),
        };
        (status, expired)
    }

    /// Signing out: frees the seat (or the queue place) at once.
    pub fn release(&mut self, session: &str, now: Instant) {
        self.active.remove(session);
        self.queue.retain(|w| w.session != session);
        self.promote(now);
    }

    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize) {
        (self.active.len(), self.queue.len())
    }
}

fn seats() -> &'static Mutex<Seats> {
    static SEATS: OnceLock<Mutex<Seats>> = OnceLock::new();
    SEATS.get_or_init(|| {
        let (max, ttl) = limits();
        tracing::info!(
            seats = max,
            minutes = ttl.as_secs() / 60,
            "demo mode: signed-in people take a timed seat"
        );
        Mutex::new(Seats::new(max, ttl))
    })
}

/// The seat of this session; the second value lists `(session, user_id)` of
/// every seat that just expired, which the caller revokes.
pub fn status(session: &str, user_id: &str) -> (SeatStatus, Vec<(String, String)>) {
    match seats().lock() {
        Ok(mut seats) => seats.status(session, user_id, Instant::now()),
        Err(_) => (SeatStatus::Expired, Vec::new()),
    }
}

/// `DEMO_GUEST_COOLDOWN` (minutes, default the session time): how long a
/// guest whose time ran out waits before the same fingerprint gets a new
/// session (`DEMO_NO_AUTH`).
pub fn guest_cooldown() -> Duration {
    static COOLDOWN: OnceLock<Duration> = OnceLock::new();
    *COOLDOWN.get_or_init(|| {
        std::env::var("DEMO_GUEST_COOLDOWN")
            .ok()
            .or_else(|| crate::brand::env("DEMO_GUEST_COOLDOWN").ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|minutes| Duration::from_secs(minutes * 60))
            .unwrap_or_else(|| limits().1)
    })
}

/// When each person's seat last ran out. In memory, like the seats.
fn ended() -> &'static Mutex<HashMap<String, Instant>> {
    static ENDED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    ENDED.get_or_init(Default::default)
}

/// Notes that this person's time ran out.
pub fn note_expired(user_id: &str) {
    if let Ok(mut ended) = ended().lock() {
        let now = Instant::now();
        let cooldown = guest_cooldown();
        ended.retain(|_, at| now.duration_since(*at) < cooldown);
        ended.insert(user_id.to_owned(), now);
    }
}

/// Seconds this person still waits before a new guest session, if any.
pub fn cooldown_left(user_id: &str) -> Option<u64> {
    let ended = ended().lock().ok()?;
    let at = ended.get(user_id)?;
    let until = *at + guest_cooldown();
    let now = Instant::now();
    (until > now).then(|| (until - now).as_secs().max(1))
}

pub fn release(session: &str) {
    if let Ok(mut seats) = seats().lock() {
        seats.release(session, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn seats_fill_then_the_rest_queue_with_an_estimate() {
        let now = Instant::now();
        let mut seats = Seats::new(2, 20 * MIN);
        assert!(matches!(seats.status("a", "ua", now).0, SeatStatus::Active { .. }));
        let later = now + 5 * MIN;
        assert!(matches!(seats.status("b", "ub", later).0, SeatStatus::Active { .. }));
        let (status, _) = seats.status("c", "uc", later);
        assert_eq!(
            status,
            SeatStatus::Queued { position: 1, waiting: 1, eta: 15 * MIN }
        );
        let (status, _) = seats.status("d", "ud", later);
        assert_eq!(
            status,
            SeatStatus::Queued { position: 2, waiting: 2, eta: 20 * MIN }
        );
        let (status, _) = seats.status("e", "ue", later);
        // A second round: the first seat to free, plus a whole session.
        assert_eq!(
            status,
            SeatStatus::Queued { position: 3, waiting: 3, eta: 35 * MIN }
        );
    }

    #[test]
    fn signing_out_frees_a_seat_for_the_head_of_the_queue() {
        let now = Instant::now();
        let mut seats = Seats::new(1, 20 * MIN);
        seats.status("a", "ua", now);
        assert!(matches!(seats.status("b", "ub", now).0, SeatStatus::Queued { .. }));
        seats.release("a", now);
        assert!(matches!(seats.status("b", "ub", now).0, SeatStatus::Active { .. }));
    }

    #[test]
    fn an_expired_seat_ends_its_session_and_passes_on() {
        let now = Instant::now();
        let mut seats = Seats::new(1, 20 * MIN);
        seats.status("a", "ua", now);
        seats.status("b", "ub", now);
        let later = now + 20 * MIN;
        seats.status("b", "ub", now + 10 * MIN);
        let (status, expired) = seats.status("a", "ua", later);
        assert_eq!(status, SeatStatus::Expired);
        assert_eq!(expired, vec![("a".to_owned(), "ua".to_owned())]);
        assert!(matches!(seats.status("b", "ub", later).0, SeatStatus::Active { .. }));
    }

    #[test]
    fn a_queue_place_nobody_polls_is_dropped() {
        let now = Instant::now();
        let mut seats = Seats::new(1, 20 * MIN);
        seats.status("a", "ua", now);
        seats.status("b", "ub", now);
        seats.status("c", "uc", now + QUEUE_STALE);
        assert_eq!(seats.counts(), (1, 1));
    }

    #[test]
    fn one_person_holds_one_seat_across_sessions() {
        let now = Instant::now();
        let mut seats = Seats::new(2, 20 * MIN);
        let (first, _) = seats.status("a", "ua", now);
        let (second, _) = seats.status("a2", "ua", now + MIN);
        assert_eq!(first, second);
        assert_eq!(seats.counts(), (1, 0));
    }
}
