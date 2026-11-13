use std::collections::VecDeque;
use std::env;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::builder::makepkg::BuildPhase;
use crate::ui::viewport::{InlineViewport, compute_viewport_height};

/// Maximum number of recent log lines retained per build node in the live UI.
pub const LOG_RING_CAPACITY: usize = 5;

/// Tick interval for spinner animation (~12.5 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(80);

/// State of a single package node in the inline build/install tree.
#[derive(Debug, Clone)]
pub struct BuildNodeState {
    pub pkgbase: String,
    pub version: String,
    pub phase: BuildPhase,
    pub phase_label: Option<String>,
    pub log_tail: VecDeque<String>,
    pub started_at: Option<Instant>,
    pub elapsed: Duration,
}

impl BuildNodeState {
    /// Creates a new node in [`BuildPhase::Pending`] with an empty log tail.
    pub fn new(pkgbase: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            pkgbase: pkgbase.into(),
            version: version.into(),
            phase: BuildPhase::Pending,
            phase_label: None,
            log_tail: VecDeque::with_capacity(LOG_RING_CAPACITY),
            started_at: None,
            elapsed: Duration::ZERO,
        }
    }

    /// Appends a log line, evicting the oldest line when at [`LOG_RING_CAPACITY`].
    pub fn push_log(&mut self, line: impl Into<String>) {
        if self.log_tail.len() >= LOG_RING_CAPACITY {
            self.log_tail.pop_front();
        }
        self.log_tail.push_back(line.into());
    }

    /// Transitions the node to `phase`, starting or freezing the elapsed timer.
    pub fn set_phase(&mut self, phase: BuildPhase) {
        self.phase_label = None;
        match phase {
            BuildPhase::Pending => {
                self.started_at = None;
                self.elapsed = Duration::ZERO;
            }
            BuildPhase::Done | BuildPhase::Failed => {
                if let Some(started) = self.started_at.take() {
                    self.elapsed = started.elapsed();
                }
            }
            _ => {
                if self.started_at.is_none() {
                    self.started_at = Some(Instant::now());
                }
            }
        }
        self.phase = phase;
    }

    /// Transitions the node to `phase` with a custom display label (e.g. for repo transactions).
    pub fn set_custom_phase(&mut self, phase: BuildPhase, label: impl Into<String>) {
        self.set_phase(phase);
        self.phase_label = Some(label.into());
    }

    /// Returns the display string for the node's current phase.
    pub fn phase_display(&self) -> &str {
        self.phase_label
            .as_deref()
            .unwrap_or_else(|| self.phase.as_str())
    }

    /// Returns the elapsed duration for display (live while active, frozen when finished).
    pub fn current_elapsed(&self) -> Duration {
        match self.started_at {
            Some(started) => started.elapsed(),
            None => self.elapsed,
        }
    }

    /// Returns `true` when the node is in an active (non-pending, non-terminal) phase.
    pub fn is_active(&self) -> bool {
        !matches!(
            self.phase,
            BuildPhase::Pending | BuildPhase::Done | BuildPhase::Failed
        )
    }
}

/// Overall progress bar state rendered at the bottom of the inline viewport.
#[derive(Debug, Clone)]
pub struct ProgressState {
    pub label: String,
    pub current: usize,
    pub total: usize,
    pub detail: Option<String>,
    pub started_at: Instant,
}

impl ProgressState {
    /// Creates a new [`ProgressState`] starting its elapsed timer now.
    pub fn new(
        label: impl Into<String>,
        current: usize,
        total: usize,
        detail: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            current,
            total,
            detail,
            started_at: Instant::now(),
        }
    }
}

/// Events sent to the UI actor loop.
#[derive(Debug)]
pub enum UiEvent {
    RegisterBase {
        pkgbase: String,
        version: String,
    },
    PhaseChanged {
        pkgbase: String,
        phase: BuildPhase,
    },
    LogLine {
        pkgbase: String,
        line: String,
    },
    SetHeader {
        title: String,
    },
    SetProgress {
        label: String,
        current: usize,
        total: usize,
        detail: Option<String>,
    },
    ClearProgress,
    RepoPhase {
        package: String,
        phase: String,
        log: Option<String>,
    },
    Suspend {
        ack: oneshot::Sender<()>,
    },
    Resume {
        ack: oneshot::Sender<()>,
    },
    Shutdown,
}

/// Shared UI state rendered by [`crate::ui::viewport::draw_tree`].
#[derive(Debug, Default, Clone)]
pub struct AppState {
    pub nodes: Vec<BuildNodeState>,
    pub header: Option<String>,
    pub progress: Option<ProgressState>,
    pub started_at: Option<Instant>,
    pub tick: usize,
    pub suspended: bool,
}

impl AppState {
    /// Creates an empty [`AppState`].
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_started(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }

    /// Registers a package base (or updates its version if already present).
    pub fn register_base(&mut self, pkgbase: impl Into<String>, version: impl Into<String>) {
        self.ensure_started();
        let pkgbase = pkgbase.into();
        let version = version.into();
        if let Some(existing) = self.nodes.iter_mut().find(|n| n.pkgbase == pkgbase) {
            existing.version = version;
        } else {
            self.nodes.push(BuildNodeState::new(pkgbase, version));
        }
    }

    /// Updates the phase of `pkgbase`, registering it first if unknown.
    pub fn set_phase(&mut self, pkgbase: &str, phase: BuildPhase) {
        self.ensure_started();
        if let Some(node) = self.nodes.iter_mut().find(|n| n.pkgbase == pkgbase) {
            node.set_phase(phase);
        } else {
            let mut node = BuildNodeState::new(pkgbase, "");
            node.set_phase(phase);
            self.nodes.push(node);
        }
    }

    /// Appends a log line to `pkgbase`, registering it first if unknown.
    pub fn push_log(&mut self, pkgbase: &str, line: impl Into<String>) {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.pkgbase == pkgbase) {
            node.push_log(line);
        } else {
            let mut node = BuildNodeState::new(pkgbase, "");
            node.push_log(line);
            self.nodes.push(node);
        }
    }

    /// Sets a custom header title for the inline viewport.
    pub fn set_header(&mut self, title: impl Into<String>) {
        self.header = Some(title.into());
    }

    /// Sets or updates the overall progress bar state, preserving `started_at` if already active.
    pub fn set_progress(
        &mut self,
        label: impl Into<String>,
        current: usize,
        total: usize,
        detail: Option<String>,
    ) {
        self.ensure_started();
        let label = label.into();
        if let Some(existing) = self.progress.as_mut() {
            existing.label = label;
            existing.current = current;
            existing.total = total;
            existing.detail = detail;
        } else {
            self.progress = Some(ProgressState::new(label, current, total, detail));
        }
    }

    /// Clears the explicit overall progress bar state.
    pub fn clear_progress(&mut self) {
        self.progress = None;
    }

    /// Updates a repo package/transaction node with a custom phase label and optional log line.
    pub fn set_repo_phase(&mut self, package: &str, phase: impl Into<String>, log: Option<String>) {
        self.ensure_started();
        let phase = phase.into();
        let idx = if let Some(idx) = self.nodes.iter().position(|n| n.pkgbase == package) {
            idx
        } else {
            self.nodes.push(BuildNodeState::new(package, ""));
            self.nodes.len() - 1
        };

        let node = &mut self.nodes[idx];
        if phase.eq_ignore_ascii_case("done") {
            node.set_phase(BuildPhase::Done);
        } else if phase.eq_ignore_ascii_case("failed") {
            node.set_phase(BuildPhase::Failed);
        } else if phase.eq_ignore_ascii_case("pending") {
            node.set_phase(BuildPhase::Pending);
        } else {
            node.set_custom_phase(BuildPhase::Installing, phase);
        }

        if let Some(line) = log {
            node.push_log(line);
        }
    }

    /// Returns the overall elapsed duration since the first recorded activity.
    pub fn elapsed(&self) -> Duration {
        self.started_at
            .map(|started| started.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// Advances the animation tick counter for Braille spinners.
    pub fn advance_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Number of nodes that have reached [`BuildPhase::Done`].
    pub fn completed_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.phase == BuildPhase::Done)
            .count()
    }

    /// Total number of registered package bases.
    pub fn total_count(&self) -> usize {
        self.nodes.len()
    }
}

/// Cloneable handle for sending progress and suspension events to the UI actor.
#[derive(Debug, Clone)]
pub struct TuiHandle {
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl TuiHandle {
    /// Registers a package base to be tracked in the build tree.
    pub fn register_base(&self, pkgbase: impl Into<String>, version: impl Into<String>) {
        let _ = self.tx.send(UiEvent::RegisterBase {
            pkgbase: pkgbase.into(),
            version: version.into(),
        });
    }

    /// Notifies the UI that `pkgbase` transitioned to `phase`.
    pub fn phase_changed(&self, pkgbase: impl Into<String>, phase: BuildPhase) {
        let _ = self.tx.send(UiEvent::PhaseChanged {
            pkgbase: pkgbase.into(),
            phase,
        });
    }

    /// Sends a single output line for `pkgbase` to the 5-line rolling tail.
    pub fn log_line(&self, pkgbase: impl Into<String>, line: impl Into<String>) {
        let _ = self.tx.send(UiEvent::LogLine {
            pkgbase: pkgbase.into(),
            line: line.into(),
        });
    }

    /// Sets a custom header title for the inline viewport.
    pub fn set_header(&self, title: impl Into<String>) {
        let _ = self.tx.send(UiEvent::SetHeader {
            title: title.into(),
        });
    }

    /// Sets or updates the overall bottom progress bar.
    pub fn set_progress(
        &self,
        label: impl Into<String>,
        current: usize,
        total: usize,
        detail: Option<String>,
    ) {
        let _ = self.tx.send(UiEvent::SetProgress {
            label: label.into(),
            current,
            total,
            detail,
        });
    }

    /// Clears the explicit bottom progress bar.
    pub fn clear_progress(&self) {
        let _ = self.tx.send(UiEvent::ClearProgress);
    }

    /// Updates a repository package/transaction node's phase and optional log line.
    pub fn repo_phase(
        &self,
        package: impl Into<String>,
        phase: impl Into<String>,
        log: Option<String>,
    ) {
        let _ = self.tx.send(UiEvent::RepoPhase {
            package: package.into(),
            phase: phase.into(),
            log,
        });
    }

    /// Suspends the inline TUI and waits until raw mode is disabled and stdout is flushed.
    ///
    /// Dropping the returned [`SuspendGuard`] automatically resumes the inline TUI.
    pub async fn suspend(&self) -> SuspendGuard {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(UiEvent::Suspend { ack: ack_tx }).is_ok() {
            let _ = ack_rx.await;
            SuspendGuard {
                tx: self.tx.clone(),
                active: true,
            }
        } else {
            SuspendGuard {
                tx: self.tx.clone(),
                active: false,
            }
        }
    }

    /// Signals the UI actor to flush and exit cleanly.
    pub fn shutdown(&self) {
        let _ = self.tx.send(UiEvent::Shutdown);
    }
}

/// RAII guard that resumes the inline TUI when dropped after an interactive subprocess exits.
#[derive(Debug)]
pub struct SuspendGuard {
    tx: mpsc::UnboundedSender<UiEvent>,
    active: bool,
}

impl Drop for SuspendGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let (ack_tx, _ack_rx) = oneshot::channel();
            let _ = self.tx.send(UiEvent::Resume { ack: ack_tx });
        }
    }
}

/// Returns `true` when the environment requires Plain Line Mode instead of an inline TUI.
pub fn should_use_plain_mode() -> bool {
    !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || env::var_os("NO_COLOR").is_some()
}

/// Spawns the UI actor task, automatically selecting Inline TUI or Plain Line Mode.
pub fn spawn_ui() -> (TuiHandle, JoinHandle<()>) {
    spawn_ui_with_mode(should_use_plain_mode())
}

/// Spawns the UI actor task with an explicit mode flag (useful for testing).
pub fn spawn_ui_with_mode(plain_mode: bool) -> (TuiHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = TuiHandle { tx };
    let join = tokio::spawn(async move {
        if plain_mode {
            run_plain_loop(rx).await;
        } else {
            run_inline_tui_loop(rx).await;
        }
    });
    (handle, join)
}

async fn run_plain_loop(mut rx: mpsc::UnboundedReceiver<UiEvent>) {
    let mut state = AppState::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            UiEvent::RegisterBase { pkgbase, version } => {
                state.register_base(pkgbase, version);
            }
            UiEvent::PhaseChanged { pkgbase, phase } => {
                state.set_phase(&pkgbase, phase);
                println!(":: [{pkgbase}] {phase}");
            }
            UiEvent::LogLine { pkgbase, line } => {
                state.push_log(&pkgbase, line);
            }
            UiEvent::SetHeader { title } => {
                state.set_header(title);
            }
            UiEvent::SetProgress {
                label,
                current,
                total,
                detail,
            } => {
                state.set_progress(label, current, total, detail);
                if let Some(p) = &state.progress {
                    if p.total > 0 {
                        match &p.detail {
                            Some(d) => println!(":: [{}/{}] {}: {d}", p.current, p.total, p.label),
                            None => println!(":: [{}/{}] {}", p.current, p.total, p.label),
                        }
                    } else if let Some(d) = &p.detail {
                        println!(":: {}: {d}", p.label);
                    }
                }
            }
            UiEvent::ClearProgress => {
                state.clear_progress();
            }
            UiEvent::RepoPhase {
                package,
                phase,
                log,
            } => {
                state.set_repo_phase(&package, phase.clone(), log);
                println!(":: [{package}] {phase}");
            }
            UiEvent::Suspend { ack } => {
                state.suspended = true;
                let _ = ack.send(());
            }
            UiEvent::Resume { ack } => {
                state.suspended = false;
                let _ = ack.send(());
            }
            UiEvent::Shutdown => break,
        }
    }
}

async fn run_inline_tui_loop(mut rx: mpsc::UnboundedReceiver<UiEvent>) {
    let mut state = AppState::new();
    let mut viewport = match InlineViewport::new(compute_viewport_height(&state)) {
        Ok(vp) => vp,
        Err(_) => {
            // Fall back to plain line mode if terminal initialization fails.
            run_plain_loop(rx).await;
            return;
        }
    };

    let _ = viewport.draw(&state);
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_ev = rx.recv() => {
                match maybe_ev {
                    None | Some(UiEvent::Shutdown) => {
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                        let _ = viewport.finish();
                        break;
                    }
                    Some(UiEvent::Suspend { ack }) => {
                        if !state.suspended {
                            let _ = viewport.suspend();
                            state.suspended = true;
                        }
                        let _ = ack.send(());
                    }
                    Some(UiEvent::Resume { ack }) => {
                        if state.suspended {
                            let height = compute_viewport_height(&state);
                            if viewport.resume(height).is_ok() {
                                state.suspended = false;
                                let _ = viewport.draw(&state);
                            }
                        }
                        let _ = ack.send(());
                    }
                    Some(UiEvent::RegisterBase { pkgbase, version }) => {
                        state.register_base(pkgbase, version);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::PhaseChanged { pkgbase, phase }) => {
                        state.set_phase(&pkgbase, phase);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::LogLine { pkgbase, line }) => {
                        state.push_log(&pkgbase, line);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::SetHeader { title }) => {
                        state.set_header(title);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::SetProgress {
                        label,
                        current,
                        total,
                        detail,
                    }) => {
                        state.set_progress(label, current, total, detail);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::ClearProgress) => {
                        state.clear_progress();
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                    Some(UiEvent::RepoPhase {
                        package,
                        phase,
                        log,
                    }) => {
                        state.set_repo_phase(&package, phase, log);
                        if !state.suspended {
                            let _ = viewport.draw(&state);
                        }
                    }
                }
            }
            _ = ticker.tick(), if !state.suspended => {
                state.advance_tick();
                let _ = viewport.draw(&state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_ring_buffer_caps_at_five_lines() {
        let mut node = BuildNodeState::new("yay-bin", "12.4.2-1");
        for i in 1..=8 {
            node.push_log(format!("line {i}"));
        }
        assert_eq!(node.log_tail.len(), LOG_RING_CAPACITY);
        let collected: Vec<&str> = node.log_tail.iter().map(String::as_str).collect();
        assert_eq!(
            collected,
            vec!["line 4", "line 5", "line 6", "line 7", "line 8"]
        );
    }

    #[test]
    fn phase_transitions_manage_timer_and_counts() {
        let mut state = AppState::new();
        state.register_base("foo", "1.0-1");
        state.register_base("bar", "2.0-1");
        assert_eq!(state.total_count(), 2);
        assert_eq!(state.completed_count(), 0);

        state.set_phase("foo", BuildPhase::Building);
        assert!(state.nodes[0].started_at.is_some());
        assert!(state.nodes[0].is_active());

        state.set_phase("foo", BuildPhase::Done);
        assert!(state.nodes[0].started_at.is_none());
        assert!(!state.nodes[0].is_active());
        assert_eq!(state.completed_count(), 1);
    }

    #[tokio::test]
    async fn plain_mode_handles_suspend_resume_and_shutdown() {
        let (handle, join) = spawn_ui_with_mode(true);
        handle.register_base("demo", "0.1.0-1");
        handle.phase_changed("demo", BuildPhase::Building);
        handle.log_line("demo", "compiling...");

        let guard = handle.suspend().await;
        drop(guard);

        handle.phase_changed("demo", BuildPhase::Done);
        handle.shutdown();
        join.await.expect("plain UI actor should exit cleanly");
    }

    #[test]
    fn progress_state_and_repo_phase_manage_counts_and_logs() {
        let mut state = AppState::new();
        state.set_header("Installing official packages");
        state.set_progress("Installing", 1, 6, Some("installing libgedit".to_string()));
        let started = state.progress.as_ref().unwrap().started_at;

        state.set_progress("Installing", 3, 6, Some("installing gedit".to_string()));
        let progress = state.progress.as_ref().unwrap();
        assert_eq!(progress.current, 3);
        assert_eq!(progress.total, 6);
        assert_eq!(progress.detail.as_deref(), Some("installing gedit"));
        assert_eq!(progress.started_at, started);

        state.set_repo_phase(
            "pacman",
            "installing",
            Some("(3/6) installing gedit...".to_string()),
        );
        assert_eq!(state.nodes.len(), 1);
        assert!(state.nodes[0].is_active());
        assert_eq!(state.nodes[0].phase_display(), "installing");
        assert_eq!(
            state.nodes[0].log_tail.back().map(String::as_str),
            Some("(3/6) installing gedit...")
        );

        state.set_repo_phase("pacman", "done", None);
        assert_eq!(state.nodes[0].phase, BuildPhase::Done);
        assert_eq!(state.nodes[0].phase_display(), "done");

        state.clear_progress();
        assert!(state.progress.is_none());
    }

    #[tokio::test]
    async fn plain_mode_handles_progress_and_repo_phase_events() {
        let (handle, join) = spawn_ui_with_mode(true);
        handle.set_header("Installing official packages");
        handle.set_progress("Installing", 0, 2, Some("retrieving packages".to_string()));
        handle.repo_phase("pacman", "downloading", Some("downloading gedit...".into()));
        handle.set_progress("Installing", 1, 2, Some("installing gedit".to_string()));
        handle.repo_phase("pacman", "done", None);
        handle.clear_progress();
        handle.shutdown();
        join.await.expect("plain UI actor should exit cleanly");
    }
}
