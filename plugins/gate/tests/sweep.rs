//! The active sweep (gate.md section 7): a reviewer that exited or ran past
//! its timeout is killed, its beads abandoned and its author mailed once,
//! with no push; a failed bd write keeps the state for the next pass. One test
//! process: state/tool locations are environment-owned.
mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use vega_gate::state::{self, Review};
use vega_gate::sweep;

const NOW: i64 = 1_800_000_000;

fn setup(tmp: &Path) {
    std::env::set_var("HOME", tmp);
    std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
    std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
    std::env::remove_var("HERDR_PANE_ID");
    std::env::remove_var("HC_TERMINAL_ID");
    for (key, value) in common::fakes(tmp) {
        std::env::set_var(key, value);
    }
}

/// An hc review of two segments (two beads), started `age` s before NOW.
fn review(tmp: &Path, pane: &str, age: i64) -> Review {
    let id = format!("rv-{pane}");
    let clone = tmp.join("cache/vega/gate/clones").join(&id);
    let work = tmp.join("cache/vega/gate").join(&id);
    fs::create_dir_all(&clone).unwrap();
    fs::create_dir_all(&work).unwrap();
    let r = Review::from_json(&serde_json::json!({
        "backend": "hc", "pane": pane, "repo": tmp, "branch": "feat",
        "head": "0123456789abcdef", "keys": ["k1", "k2"],
        "beads": [format!("{pane}-b1"), format!("{pane}-b2")], "review": id,
        "author_pane": "t1", "clone": clone, "work": work,
        "kind": "agy", "model": "gemini-3.7-flash", "tab": "gate-feat",
        "started_at": NOW - age, "timeout_secs": 1200,
    }))
    .unwrap();
    r.save().unwrap();
    r
}

fn terminal(tmp: &Path, pane: &str, state: &str) {
    fs::write(
        tmp.join(format!("hc-{pane}.json")),
        serde_json::json!({"address": pane, "state": state, "argv": [], "labels": {},
            "cwd": "/fixture", "created_ms": 0, "screen": "exact", "cols": 80, "rows": 24,
            "status": {"state": "idle"}, "turn": 0})
        .to_string(),
    )
    .unwrap();
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

fn hung_reviewer_is_killed_abandoned_and_mailed_once_without_a_push(tmp: &Path) {
    let r = review(tmp, "t29", 1300);
    terminal(tmp, "t29", "live");
    let fresh = review(tmp, "t30", 600);
    terminal(tmp, "t30", "live");
    let young = review(tmp, "t31", 5);
    terminal(tmp, "t31", "finished");

    let swept = sweep::run(NOW, |_| true);
    assert_eq!(swept.len(), 1, "{swept:?}");
    let s = &swept[0];
    assert_eq!(s.pane, "t29");
    assert_eq!(s.reason, "reviewer t29 silent past the 1200 s timeout");
    assert!(s.done && s.mailed && s.error.is_none(), "{s:?}");

    let hc = common::log(tmp, "hc");
    assert!(hc.contains("kill t29"), "{hc}");
    assert!(hc.contains("rm t29"), "{hc}");
    assert_eq!(
        count(&hc, "send t1 gate review of feat@0123456"),
        1,
        "{hc}\n{swept:?}"
    );
    assert!(hc.contains("silent past the 1200 s timeout; no verdict. Next: push again"));
    let bd = common::log(tmp, "bd");
    for b in ["t29-b1", "t29-b2"] {
        assert!(bd.contains(&format!("update {b} --set-metadata verdict=abandoned")));
        assert!(bd.contains(&format!(
            "close {b} --reason abandoned: reviewer t29 silent past the 1200 s timeout"
        )));
    }
    assert!(state::load("t29").is_none());
    assert!(!r.clone.exists() && !r.work.exists());
    // Alive and in time, or within the grace: untouched.
    for other in [&fresh, &young] {
        assert_eq!(state::load(&other.pane).as_ref(), Some(other));
        assert!(!hc.contains(&format!("kill {}", other.pane)));
    }
    state::remove(&fresh.pane);
    state::remove(&young.pane);
}

fn bd_down_keeps_the_state_and_retries_without_a_second_mail(tmp: &Path) {
    let r = review(tmp, "t40", 100);
    terminal(tmp, "t40", "finished");
    // The first bead finishes, then the Dolt server goes away.
    fs::write(tmp.join("bd-down-t40-b2"), "").unwrap();

    let first = sweep::run(NOW, |_| true);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].reason, "reviewer t40 exited without a verdict");
    assert!(!first[0].done && first[0].mailed, "{:?}", first[0]);
    assert!(first[0]
        .error
        .as_deref()
        .unwrap()
        .contains("connection refused"));
    let kept = state::load("t40").expect("state kept while a bead is running");
    assert_eq!(kept.beads, vec!["t40-b2".to_string()]);
    assert_eq!(kept.ended, "reviewer t40 exited without a verdict");
    assert!(kept.mailed);
    assert!(r.clone.exists(), "clone kept with the state");
    assert!(
        !common::log(tmp, "hc").contains("kill t40"),
        "an exited reviewer is not killed"
    );
    // Not an author waiting for a verdict any more.
    assert!(!state::waiting_authors(NOW).contains("t1"));

    // Still down: retried, not re-mailed; the reason does not change even
    // though the terminal record is gone now.
    let second = sweep::run(NOW + 60, |_| true);
    assert!(!second[0].done && !second[0].mailed, "{:?}", second[0]);
    assert_eq!(second[0].reason, "reviewer t40 exited without a verdict");

    // bd is back: the last bead closes and only then the state goes.
    fs::remove_file(tmp.join("bd-down-t40-b2")).unwrap();
    let third = sweep::run(NOW + 120, |_| true);
    assert!(third[0].done && !third[0].mailed, "{:?}", third[0]);
    assert!(state::load("t40").is_none());
    assert!(!r.clone.exists());
    let bd = common::log(tmp, "bd");
    assert_eq!(count(&bd, "close t40-b1"), 1, "{bd}");
    assert_eq!(count(&bd, "close t40-b2"), 1, "{bd}");
    assert_eq!(
        count(&common::log(tmp, "hc"), "send t1"),
        2,
        "one mail per review"
    );
    assert!(sweep::run(NOW + 180, |_| true).is_empty());
}

fn gc_keeps_a_day_old_review_whose_beads_are_open(tmp: &Path) {
    let r = review(tmp, "t50", 2 * 86_400);
    // Sweep has not run yet and bd says the beads are open: gc must not drop
    // the only handle on them, whatever the age.
    let mut old = r.clone();
    old.started_at = vega_shim::unix_now() - 2 * 86_400;
    old.save().unwrap();
    fs::write(tmp.join("bd-open"), "").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_vega-gate"))
        .arg("gc")
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    assert!(state::load("t50").is_some(), "{o:?}");
    fs::remove_file(tmp.join("bd-open")).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_vega-gate"))
        .arg("gc")
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    assert!(state::load("t50").is_none(), "closed beads: gc removes it");
}

#[test]
fn the_sweep_ends_hung_and_exited_reviews_and_never_orphans_running_beads() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path());
    hung_reviewer_is_killed_abandoned_and_mailed_once_without_a_push(tmp.path());
    bd_down_keeps_the_state_and_retries_without_a_second_mail(tmp.path());
    gc_keeps_a_day_old_review_whose_beads_are_open(tmp.path());
}
