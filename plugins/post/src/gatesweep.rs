//! vegad's gate sweep thread (docs/design/gate.md section 7): ends gate
//! reviews on this host whose reviewer exited or ran past its timeout, so a
//! hung reviewer does not wait for the author's next push. It kills hc
//! terminals and calls bd (5 s each), so it runs here, never on the relay
//! loop; the loop's budgets are untouched. It only logs.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

/// A review ends at most this long after its timeout or its reviewer's exit.
const EVERY: Duration = Duration::from_secs(60);

pub fn spawn() {
    thread::spawn(|| {
        // A review whose bd stays down is retried every pass; log it once
        // per distinct line, not every minute.
        let mut said: HashMap<String, String> = HashMap::new();
        loop {
            for s in vega_gate::sweep::run(vega_ci_watch::unix_now(), |_| true) {
                let line = s.line();
                if said.get(&s.pane) != Some(&line) {
                    crate::log(&line);
                }
                if s.done {
                    said.remove(&s.pane);
                } else {
                    said.insert(s.pane, line);
                }
            }
            thread::sleep(EVERY);
        }
    });
}
