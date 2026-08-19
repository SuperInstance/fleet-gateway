//! Phase 3 acceptance: a breaker shared across clients.
//!
//! This is the whole reason the gateway is a *service* rather than a library:
//! every client talks to the same per-provider `CircuitBreaker`, so when one
//! client's failures trip a provider OPEN, every other concurrent client
//! observes the OPEN state and gets rejected immediately — no thundering herd
//! against a sick upstream, no per-client blind spot.
//!
//! Deterministic and network-free: two OS threads share one
//! `Arc<CircuitBreaker>`. The observer thread is provably polling *before*
//! the offender trips the breaker (enforced with a channel handshake), and we
//! assert the observer sees `Open` within one poll interval of the trip.

use fleet_gateway::circuit_breaker::{BreakerState, CircuitBreaker};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// How often the observer client polls the shared breaker.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Upper bound for "observed OPEN within one poll interval".
/// The physical guarantee is the *next* poll after the trip (~1 ms); we allow
/// generous slack for CI scheduling jitter so this never flakes while still
/// proving the observation is effectively immediate.
const OBSERVE_DEADLINE: Duration = Duration::from_millis(500);

/// Hard wall for the whole test so a broken breaker can't hang CI.
const TEST_WALL: Duration = Duration::from_secs(5);

/// What the observer client saw while polling.
#[derive(Debug)]
struct Observation {
    /// Polls that returned Closed before the first Open was seen.
    polls_while_closed: u32,
    /// Whether an Open state was ever observed.
    saw_open: bool,
    /// `allow_request()` result at the moment Open was first observed.
    allowed_when_open: Option<bool>,
    /// When Open was first observed (None if never).
    observed_at: Option<Instant>,
}

#[test]
fn second_client_observes_open_after_first_trips_shared_breaker() {
    // One breaker, exactly like one provider in the live gateway.
    // threshold=5 failures to trip; cooldown 600s so no HalfOpen transition
    // can race the observation window.
    let breaker = Arc::new(CircuitBreaker::new(5, 600, 2));

    // Handshake: observer signals it is polling; offender reports trip time.
    let (observer_ready_tx, observer_ready_rx) = mpsc::channel::<()>();
    let (tripped_tx, tripped_rx) = mpsc::channel::<Instant>();
    let (observed_tx, observed_rx) = mpsc::channel::<Observation>();

    let polls_while_closed = Arc::new(AtomicU32::new(0));

    // ------------------------------------------------------------------
    // Client B — the second, innocent client. Starts polling immediately
    // and keeps polling at POLL_INTERVAL until it sees Open.
    // ------------------------------------------------------------------
    let breaker_b = Arc::clone(&breaker);
    let polls_b = Arc::clone(&polls_while_closed);
    let observer = thread::spawn(move || {
        // CircuitBreaker is async (tokio Mutex) — give this OS thread its own
        // single-threaded runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("observer runtime");

        let obs = rt.block_on(async {
            let mut saw_open = false;
            let mut allowed_when_open = None;
            let mut observed_at = None;
            let mut polls_while_closed = 0u32;
            let deadline = Instant::now() + TEST_WALL;

            // Signal AFTER entering the loop context: we are about to poll.
            // (Sent before the first poll so the offender cannot race ahead.)
            observer_ready_tx.send(()).expect("ready send");

            while Instant::now() < deadline {
                if breaker_b.state().await == BreakerState::Open {
                    saw_open = true;
                    allowed_when_open = Some(breaker_b.allow_request().await);
                    observed_at = Some(Instant::now());
                    break;
                }
                polls_while_closed += 1;
                tokio::time::sleep(POLL_INTERVAL).await;
            }

            polls_b.store(polls_while_closed, Ordering::SeqCst);
            Observation {
                polls_while_closed,
                saw_open,
                allowed_when_open,
                observed_at,
            }
        });

        observed_tx.send(obs).expect("observation send");
    });

    // Wait until client B is genuinely polling the shared breaker.
    observer_ready_rx
        .recv_timeout(TEST_WALL)
        .expect("observer never started polling");

    // ------------------------------------------------------------------
    // Client A — the offender. Hammers the provider until the shared
    // breaker trips (5 consecutive failures).
    // ------------------------------------------------------------------
    let breaker_a = Arc::clone(&breaker);
    let offender = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("offender runtime");

        let tripped_at = rt.block_on(async {
            for _ in 0..5 {
                breaker_a.record_failure().await;
            }
            // The trip is complete the moment the 5th failure lands.
            Instant::now()
        });

        // Publish the trip timestamp so the main thread can measure how long
        // the observer took to see OPEN.
        tripped_tx.send(tripped_at).expect("trip send");
    });

    let tripped_at = tripped_rx.recv_timeout(TEST_WALL).expect("trip signal");

    // Join everything and collect the observation.
    let observation = observed_rx
        .recv_timeout(TEST_WALL)
        .expect("observation never arrived");
    observer.join().expect("observer panicked");
    offender.join().expect("offender panicked");

    // ---------------- assertions ----------------
    // 1. The second client DID see the breaker flip Open.
    assert!(observation.saw_open, "second client never observed OPEN: {observation:?}");

    // 2. It was already polling before the trip — this is a *concurrent*
    //    observation, not a post-hoc check.
    assert!(
        observation.polls_while_closed >= 1,
        "observer never polled while CLOSED; concurrency not established: {:?}",
        observation
    );

    // 3. The operational meaning of OPEN: the second client's requests are
    //    rejected too.
    assert_eq!(
        observation.allowed_when_open,
        Some(false),
        "breaker claimed OPEN but still allowed the second client's request"
    );

    // 4. It saw Open within one poll interval (+ slack) of the trip.
    let elapsed = observation
        .observed_at
        .expect("observed_at set when saw_open")
        .duration_since(tripped_at);
    assert!(
        elapsed <= OBSERVE_DEADLINE,
        "second client took {:?} to observe OPEN (deadline {:?})",
        elapsed,
        OBSERVE_DEADLINE
    );
    assert!(
        elapsed >= Duration::ZERO,
        "observation timestamp predates the trip (test bug)"
    );
}

/// The shared view is symmetric: after the trip, a *fresh* client (one that
/// never touched the breaker before) also sees Open and is rejected —
/// i.e. the state is process-global per provider, not per-caller.
#[test]
fn fresh_client_sees_open_immediately() {
    let breaker = Arc::new(CircuitBreaker::new(3, 600, 2));

    // Client A trips it.
    let breaker_a = Arc::clone(&breaker);
    let offender = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("offender runtime");
        rt.block_on(async {
            for _ in 0..3 {
                breaker_a.record_failure().await;
            }
        });
    });
    offender.join().expect("offender panicked");

    // Client B, fresh, checks after the fact.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fresh client runtime");
    rt.block_on(async {
        assert_eq!(breaker.state().await, BreakerState::Open);
        assert!(!breaker.allow_request().await, "fresh client must be rejected while OPEN");
    });
}
