use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

#[test]
fn terminal_publication_has_exactly_one_winner() {
    loom::model(|| {
        let terminal = Arc::new(AtomicUsize::new(0));
        let wins = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for proposed in [1, 2] {
            let terminal = Arc::clone(&terminal);
            let wins = Arc::clone(&wins);
            threads.push(thread::spawn(move || {
                if terminal
                    .compare_exchange(0, proposed, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    wins.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(matches!(terminal.load(Ordering::Acquire), 1 | 2));
        assert_eq!(wins.load(Ordering::Relaxed), 1);
    });
}

#[derive(Default)]
struct CancellationState {
    reason_set: bool,
    outcome_claimed: bool,
}

#[test]
fn cancellation_and_business_outcome_linearize() {
    loom::model(|| {
        let state = Arc::new(Mutex::new(CancellationState::default()));
        let cancel_won = Arc::new(AtomicUsize::new(0));
        let outcome_won = Arc::new(AtomicUsize::new(0));

        let cancel = {
            let state = Arc::clone(&state);
            let cancel_won = Arc::clone(&cancel_won);
            thread::spawn(move || {
                let mut state = state.lock().unwrap();
                if !state.reason_set && !state.outcome_claimed {
                    state.reason_set = true;
                    cancel_won.store(1, Ordering::Release);
                }
            })
        };
        let outcome = {
            let state = Arc::clone(&state);
            let outcome_won = Arc::clone(&outcome_won);
            thread::spawn(move || {
                let mut state = state.lock().unwrap();
                if !state.reason_set {
                    state.outcome_claimed = true;
                    outcome_won.store(1, Ordering::Release);
                }
            })
        };
        cancel.join().unwrap();
        outcome.join().unwrap();

        assert_eq!(
            cancel_won.load(Ordering::Acquire) + outcome_won.load(Ordering::Acquire),
            1
        );
    });
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Owner {
    Old,
    New,
}

#[test]
fn late_old_completion_cannot_remove_replacement() {
    loom::model(|| {
        let current = Arc::new(Mutex::new(Some(Owner::Old)));
        let replacement = {
            let current = Arc::clone(&current);
            thread::spawn(move || *current.lock().unwrap() = Some(Owner::New))
        };
        let old_completion = {
            let current = Arc::clone(&current);
            thread::spawn(move || {
                let mut current = current.lock().unwrap();
                if *current == Some(Owner::Old) {
                    *current = None;
                }
            })
        };
        replacement.join().unwrap();
        old_completion.join().unwrap();
        assert!(*current.lock().unwrap() == Some(Owner::New));
    });
}

#[derive(Default)]
struct SubmitShutdownState {
    open: bool,
    accepted: bool,
    cancelled_by_shutdown: bool,
}

#[test]
fn shutdown_and_submit_share_one_linearization_gate() {
    loom::model(|| {
        let state = Arc::new(Mutex::new(SubmitShutdownState {
            open: true,
            ..SubmitShutdownState::default()
        }));
        let submit = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                let mut state = state.lock().unwrap();
                if state.open {
                    state.accepted = true;
                }
            })
        };
        let shutdown = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                let mut state = state.lock().unwrap();
                state.open = false;
                state.cancelled_by_shutdown = state.accepted;
            })
        };
        submit.join().unwrap();
        shutdown.join().unwrap();
        let state = state.lock().unwrap();
        assert!(!state.accepted || state.cancelled_by_shutdown);
    });
}
