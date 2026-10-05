//! The "exits are not active" line is written when the reason changes, not on every poll.

use rome_zk_exit_prover::run::WaitingLog;

#[test]
fn ten_polls_with_the_same_reason_log_once() {
    let mut log = WaitingLog::default();
    let logged = (0..10)
        .filter(|_| log.should_log("the exit config names no portal yet"))
        .count();
    assert_eq!(logged, 1);
}

#[test]
fn a_new_reason_logs_again() {
    let mut log = WaitingLog::default();
    assert!(log.should_log("a"));
    assert!(!log.should_log("a"));
    assert!(log.should_log("b"));
    assert!(!log.should_log("b"));
}

#[test]
fn waiting_again_after_being_active_logs_again() {
    let mut log = WaitingLog::default();
    assert!(log.should_log("a"));
    log.active();
    assert!(log.should_log("a"));
}
