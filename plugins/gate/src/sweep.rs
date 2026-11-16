//! The active end of a review without a verdict (gate.md section 7): a
//! reviewer that exited, or that runs past its `timeout`, is killed, its
//! beads are finished `abandoned` with the reason, and its author is mailed
//! once. vegad runs [`run`] on its own thread; `vega gate` (home) runs it for
//! the current repo. Nobody has to push for a hung review to end.
//!
//! The state file is the only handle on the beads: it is removed only after
//! every bead is finished. A failed bd write (bd or its server down) keeps the
//! state with the remaining beads, and the next pass retries.

use crate::record;
use crate::segments::short;
use crate::start::{cleanup, is_sentinel, pane_alive, release_pane};
use crate::state::{self, Review};
use crate::verdict::mail_author;

/// A fresh reviewer is not judged dead before its pane has had this long to
/// show up in the terminal's list.
pub const GRACE_SECS: i64 = 15;

/// One review this pass ended or retried.
#[derive(Debug, PartialEq)]
pub struct Swept {
    pub pane: String,
    pub reason: String,
    /// Every bead finished and the state removed.
    pub done: bool,
    /// The author was mailed in this pass.
    pub mailed: bool,
    /// The bd error that kept the state, when not done.
    pub error: Option<String>,
}

impl Swept {
    /// One log line for vegad and `vega gate`.
    pub fn line(&self) -> String {
        match &self.error {
            None => format!("gate: abandoned review of {}: {}", self.pane, self.reason),
            Some(e) => format!(
                "gate: review of {} ended ({}), beads not yet finished, retrying next pass: {e}",
                self.pane, self.reason
            ),
        }
    }
}

/// Why the review must end now, if it must. A review already ended by an
/// earlier pass keeps its reason. `alive` is asked only when needed.
pub fn due(r: &Review, now: i64, alive: impl FnOnce() -> bool) -> Option<String> {
    if !r.ended.is_empty() {
        return Some(r.ended.clone());
    }
    let age = now - r.started_at;
    if age <= GRACE_SECS {
        return None;
    }
    if !alive() {
        return Some(format!("reviewer {} exited without a verdict", r.pane));
    }
    (age > r.timeout_secs).then(|| {
        format!(
            "reviewer {} silent past the {} s timeout",
            r.pane, r.timeout_secs
        )
    })
}

/// Ends every due review that `which` selects. Returns what it did.
pub fn run(now: i64, which: impl Fn(&Review) -> bool) -> Vec<Swept> {
    state::all()
        .into_iter()
        .filter(|r| which(r))
        .filter_map(|r| end(&r, now))
        .collect()
}

/// Ends one review when due. The state lock decides against a verdict in
/// flight: a review whose state changed since it was read is left alone.
fn end(r: &Review, now: i64) -> Option<Swept> {
    let mut alive = None;
    let reason = due(r, now, || *alive.insert(pane_alive(r)))?;
    let guard = state::lock_current(r).ok()??;
    let mut next = r.clone();
    let mut mailed = false;
    if r.ended.is_empty() {
        next.ended = reason.clone();
        // Saved first: a crash after the kill still knows why it ended.
        let _ = next.save();
        // A timed-out reviewer is still running: stop it before its beads
        // close, so a late verdict cannot race the abandon.
        release_pane(r, alive.unwrap_or(true));
    }
    if !next.mailed {
        mailed = mail_author(&next, &mail_text(&next, &reason));
        // Once: a failed mail is not retried (the author may be gone too).
        next.mailed = true;
    }
    let abandon = format!("abandoned: {reason}");
    let mut error = None;
    next.beads.retain(|b| {
        if error.is_some() {
            return true;
        }
        match record::finish(&next.repo, b, "abandoned", &abandon, None) {
            Ok(()) => false,
            Err(e) => {
                error = Some(e);
                true
            }
        }
    });
    let done = next.beads.is_empty();
    if done {
        state::remove(&r.pane);
    } else {
        let _ = next.save();
    }
    drop(guard);
    if done {
        cleanup(&next, false);
    }
    Some(Swept {
        pane: r.pane.clone(),
        reason,
        done,
        mailed,
        error,
    })
}

/// The author's one mail about a review that ended without a verdict.
pub fn mail_text(r: &Review, reason: &str) -> String {
    format!(
        "gate review of {}@{} ({}/{}) abandoned: {reason}; no verdict. Next: {}; the next start picks another reviewer. Nothing to reply.",
        r.branch,
        short(&r.head),
        r.kind,
        r.model,
        if is_sentinel(&r.author_pane) {
            "vega gate start again"
        } else {
            "push again"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(started_at: i64) -> Review {
        Review::from_json(&serde_json::json!({
            "pane": "t29", "repo": "/r", "clone": "/c", "started_at": started_at,
            "timeout_secs": 1200,
        }))
        .unwrap()
    }

    #[test]
    fn due_after_grace_when_exited_or_past_timeout() {
        let r = review(1000);
        assert_eq!(due(&r, 1010, || false), None, "within the grace");
        assert_eq!(
            due(&r, 1100, || false).as_deref(),
            Some("reviewer t29 exited without a verdict")
        );
        assert_eq!(due(&r, 1100, || true), None, "alive, in time");
        assert_eq!(
            due(&r, 2201, || true).as_deref(),
            Some("reviewer t29 silent past the 1200 s timeout")
        );
        let mut ended = r;
        ended.ended = "earlier reason".into();
        assert_eq!(
            due(&ended, 1001, || panic!("not asked")).as_deref(),
            Some("earlier reason")
        );
    }
}
