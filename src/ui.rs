//! Small terminal-UI helpers to give the interactive session a claude/codex-like
//! feel: a styled input bar and a live "worked for Xm Ys" spinner.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Clear the current line and return the cursor to its start — printed before a
/// status line so it does not collide with the live spinner line.
pub(crate) const CLEAR_LINE: &str = "\r\x1b[2K";

/// Format a duration like `1m 04s` or `9s`.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The styled input prompt: a grey bar with a cyan chevron.
pub(crate) fn print_prompt() {
    // grey " fractal " bar, then a bright chevron the user types after.
    print!("\x1b[48;5;238m\x1b[38;5;253m fractal \x1b[0m\x1b[38;5;44m ❯\x1b[0m ");
    let _ = std::io::stdout().flush();
}

/// A background spinner that redraws `label · worked for Xm Ys` in place.
pub(crate) struct Spinner {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    start: Instant,
}

impl Spinner {
    /// Start the spinner with a label (e.g. "working").
    pub(crate) fn start(label: &str) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let start = Instant::now();
        let flag = running.clone();
        let label = label.to_owned();
        let handle = std::thread::spawn(move || {
            let mut frame = 0usize;
            while flag.load(Ordering::Relaxed) {
                let elapsed = format_elapsed(start.elapsed());
                print!(
                    "{CLEAR_LINE}\x1b[90m{} {label} · worked for {elapsed}\x1b[0m",
                    FRAMES[frame % FRAMES.len()]
                );
                let _ = std::io::stdout().flush();
                frame += 1;
                std::thread::sleep(Duration::from_millis(120));
            }
        });
        Self {
            running,
            handle: Some(handle),
            start,
        }
    }

    /// Stop the spinner, clear its line, and return the total elapsed time.
    pub(crate) fn stop(mut self) -> Duration {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        print!("{CLEAR_LINE}");
        let _ = std::io::stdout().flush();
        self.start.elapsed()
    }
}
