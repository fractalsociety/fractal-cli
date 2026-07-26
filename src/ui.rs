//! Small terminal-UI helpers to give the interactive session a claude/codex-like
//! feel: a styled input bar and a live "worked for Xm Ys" spinner.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
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

/// A low-frequency status heartbeat for agent calls that can legitimately take
/// several minutes without producing terminal output.
pub(crate) struct ProgressHeartbeat {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl ProgressHeartbeat {
    pub(crate) fn planning(lead: &str, source: &str, workspace: &Path) -> Self {
        let interval = std::env::var("FRACTAL_PLANNING_HEARTBEAT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(15));
        Self::planning_with_interval(lead, source, workspace, interval)
    }

    fn planning_with_interval(
        lead: &str,
        source: &str,
        workspace: &Path,
        interval: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let lead = lead.to_owned();
        let source = source.to_owned();
        let workspace = workspace.to_path_buf();
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let mut tick = 0usize;
            loop {
                match receiver.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let message = planning_message(tick, &lead, &source);
                        let elapsed = started.elapsed();
                        println!("  {} · {}", message, format_elapsed(elapsed));
                        let _ = std::io::stdout().flush();
                        if crate::project_file::update_planning_progress(
                            &workspace,
                            &message,
                            tick as u32 + 1,
                            elapsed.as_secs(),
                            &lead,
                            &source,
                        )
                        .is_ok()
                        {
                            crate::project_sync::maybe_sync_runtime(&workspace);
                        }
                        tick += 1;
                    }
                }
            }
        });
        Self {
            stop: Some(sender),
            handle: Some(handle),
        }
    }

    pub(crate) fn stop(mut self) {
        if let Some(sender) = self.stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ProgressHeartbeat {
    fn drop(&mut self) {
        if let Some(sender) = self.stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn planning_message(tick: usize, lead: &str, source: &str) -> String {
    const STEPS: [&str; 5] = [
        "is still reading the source and identifying requirements",
        "is selecting the architecture and component boundaries",
        "is defining acceptance criteria and verification evidence",
        "is decomposing the work into dependency-ordered tasks",
        "is checking the proposed graph for coverage and conflicts",
    ];
    format!("⏳ [{lead}] {} ({source})", STEPS[tick % STEPS.len()])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_heartbeat_rotates_through_concrete_progress_messages() {
        let first = planning_message(0, "claude", "APP_PRD.md");
        let second = planning_message(1, "claude", "APP_PRD.md");
        let wrapped = planning_message(5, "claude", "APP_PRD.md");
        assert!(first.contains("identifying requirements"));
        assert!(second.contains("selecting the architecture"));
        assert_eq!(first, wrapped);
        assert!(first.contains("[claude]"));
        assert!(first.contains("APP_PRD.md"));
    }
}
