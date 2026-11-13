use std::io::{self, Stdout, Write};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::builder::makepkg::BuildPhase;
use crate::ui::app::{AppState, BuildNodeState, LOG_RING_CAPACITY};

/// Braille spinner frames for active build nodes.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Minimum height of the inline viewport in terminal rows.
const MIN_VIEWPORT_HEIGHT: u16 = 4;

/// Maximum height of the inline viewport in terminal rows.
const MAX_VIEWPORT_HEIGHT: u16 = 24;

/// Computes an appropriate inline viewport height for the current [`AppState`].
pub fn compute_viewport_height(state: &AppState) -> u16 {
    // 1 header line + 1 line per node + up to LOG_RING_CAPACITY lines for active/failed nodes
    // + 1 bottom progress bar line when progress or nodes are active.
    let mut rows: usize = 1;
    for node in &state.nodes {
        rows += 1;
        if node.is_active() || node.phase == BuildPhase::Failed {
            rows += node.log_tail.len().min(LOG_RING_CAPACITY);
        }
    }
    if state.progress.is_some() || !state.nodes.is_empty() {
        rows += 1;
    }
    let clamped = rows.clamp(MIN_VIEWPORT_HEIGHT as usize, MAX_VIEWPORT_HEIGHT as usize);
    clamped as u16
}

/// Formats a duration as `M:SS` (e.g. `0:14`, `2:05`).
pub fn format_elapsed(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

struct EffectiveProgress<'a> {
    label: &'a str,
    current: usize,
    total: usize,
    detail: Option<String>,
    elapsed: Duration,
}

/// Renders the `nom`-style build/install tree and `nh`-style bottom progress bar into `area`.
pub fn draw_tree(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let effective_progress = if let Some(progress) = &state.progress {
        Some(EffectiveProgress {
            label: progress.label.as_str(),
            current: progress.current,
            total: progress.total,
            detail: progress.detail.clone(),
            elapsed: progress.started_at.elapsed(),
        })
    } else if !state.nodes.is_empty() {
        let active_detail = state
            .nodes
            .iter()
            .find(|n| n.is_active())
            .map(|n| format!("{} {}", n.phase_display(), n.pkgbase));
        Some(EffectiveProgress {
            label: state.header.as_deref().unwrap_or("Building AUR packages"),
            current: state.completed_count(),
            total: state.total_count(),
            detail: active_detail,
            elapsed: state.elapsed(),
        })
    } else {
        None
    };

    let (tree_area, bar_area) = if effective_progress.is_some() && area.height >= 2 {
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    let header_title = state
        .header
        .as_deref()
        .or_else(|| {
            if state.nodes.is_empty() {
                state.progress.as_ref().map(|p| p.label.as_str())
            } else {
                None
            }
        })
        .unwrap_or("Building AUR packages");

    let mut header_spans = vec![Span::styled(
        format!("{header_title} "),
        Style::default().add_modifier(Modifier::BOLD),
    )];

    if !state.nodes.is_empty() {
        header_spans.push(Span::styled(
            format!("[{}/{}]", state.completed_count(), state.total_count()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(progress) = &state.progress
        && progress.total > 0
    {
        header_spans.push(Span::styled(
            format!("[{}/{}]", progress.current, progress.total),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    lines.push(Line::from(header_spans));

    for node in &state.nodes {
        lines.push(render_node_line(node, state.tick));

        if node.is_active() || node.phase == BuildPhase::Failed {
            for log_line in node.log_tail.iter().take(LOG_RING_CAPACITY) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(log_line.clone(), Style::default().fg(Color::Gray)),
                ]));
            }
        }
    }

    let widget = Paragraph::new(lines);
    frame.render_widget(widget, tree_area);

    if let (Some(bar_area), Some(progress)) = (bar_area, effective_progress) {
        render_progress_bar(frame, bar_area, &progress, state.tick);
    }
}

fn render_progress_bar(
    frame: &mut Frame,
    area: Rect,
    progress: &EffectiveProgress<'_>,
    tick: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (count_str, is_complete) = if progress.total > 0 {
        let clamped = progress.current.min(progress.total);
        let pct = (clamped * 100) / progress.total;
        (
            format!(" {clamped}/{} ({pct}%)", progress.total),
            clamped >= progress.total,
        )
    } else {
        (String::new(), false)
    };

    let elapsed_str = format!(" · {}", format_elapsed(progress.elapsed));
    let raw_detail = progress
        .detail
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(progress.label);
    let detail_str = if raw_detail.is_empty() {
        String::new()
    } else {
        format!(" · {raw_detail}")
    };

    let total_width = area.width as usize;
    let fixed_len = count_str.chars().count() + elapsed_str.chars().count();
    let available = total_width.saturating_sub(fixed_len);
    let min_bar = 8.min(total_width / 3).max(1);
    let max_bar = 24;
    let bar_width = available
        .saturating_sub(detail_str.chars().count())
        .clamp(min_bar, max_bar);

    let max_detail = available.saturating_sub(bar_width);
    let truncated_detail = truncate_chars(&detail_str, max_detail);

    let mut spans = build_bar_spans(bar_width, progress.current, progress.total, tick);

    if !count_str.is_empty() {
        let count_color = if is_complete {
            Color::Green
        } else {
            Color::Cyan
        };
        spans.push(Span::styled(
            count_str,
            Style::default()
                .fg(count_color)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        elapsed_str,
        Style::default().fg(Color::DarkGray),
    ));

    if !truncated_detail.is_empty() {
        spans.push(Span::styled(
            truncated_detail,
            Style::default().fg(Color::Gray),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn build_bar_spans(
    bar_width: usize,
    current: usize,
    total: usize,
    tick: usize,
) -> Vec<Span<'static>> {
    if bar_width == 0 {
        return Vec::new();
    }

    if total > 0 {
        let clamped = current.min(total);
        if clamped >= total {
            return vec![Span::styled(
                "━".repeat(bar_width),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )];
        }

        let filled = ((bar_width as f64) * (clamped as f64) / (total as f64)).round() as usize;
        let filled = filled.min(bar_width);
        if filled == 0 {
            return vec![Span::styled(
                "─".repeat(bar_width),
                Style::default().fg(Color::DarkGray),
            )];
        }

        let has_tip = filled < bar_width;
        let empty = bar_width.saturating_sub(filled + usize::from(has_tip));
        let mut spans = vec![Span::styled(
            "━".repeat(filled),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        if has_tip {
            spans.push(Span::styled("╸", Style::default().fg(Color::Cyan)));
        }
        if empty > 0 {
            spans.push(Span::styled(
                "─".repeat(empty),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans
    } else {
        // Indeterminate sliding pulse when total is unknown (e.g. retrieving packages).
        let pulse_len = 3.min(bar_width);
        let pos = tick % bar_width;
        let mut spans = Vec::new();
        let left = pos;
        let right = bar_width.saturating_sub(pos + pulse_len);
        if left > 0 {
            spans.push(Span::styled(
                "─".repeat(left),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled(
            "━".repeat(pulse_len.min(bar_width - left)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        if right > 0 {
            spans.push(Span::styled(
                "─".repeat(right),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn render_node_line(node: &BuildNodeState, tick: usize) -> Line<'static> {
    let (icon, icon_style, badge_style) = match node.phase {
        BuildPhase::Pending => (
            "·",
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::DarkGray),
        ),
        BuildPhase::Done => (
            "✔",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Green),
        ),
        BuildPhase::Failed => (
            "✖",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        _ => (
            SPINNER_FRAMES[tick % SPINNER_FRAMES.len()],
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Yellow),
        ),
    };

    let mut spans = vec![
        Span::styled(format!(" {icon} "), icon_style),
        Span::styled(
            node.pkgbase.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];

    if !node.version.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            node.version.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("[{}]", node.phase_display()),
        badge_style,
    ));

    if node.phase != BuildPhase::Pending {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format_elapsed(node.current_elapsed()),
            Style::default().fg(Color::DarkGray),
        ));
    }

    Line::from(spans)
}

/// Manages a `ratatui` `Viewport::Inline` terminal with clean suspend/resume semantics.
///
/// Never uses an alternate screen buffer so terminal scrollback is preserved.
pub struct InlineViewport {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    height: u16,
}

impl InlineViewport {
    /// Enables raw mode and initializes an inline `ratatui` viewport of `height` rows.
    pub fn new(height: u16) -> io::Result<Self> {
        let terminal = create_inline_terminal(height)?;
        Ok(Self {
            terminal: Some(terminal),
            height,
        })
    }

    /// Draws the current [`AppState`] into the inline viewport, dynamically resizing if needed.
    pub fn draw(&mut self, state: &AppState) -> io::Result<()> {
        let desired_height = compute_viewport_height(state);
        if desired_height != self.height {
            self.resize(desired_height)?;
        }
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.draw(|frame| draw_tree(frame, frame.area(), state))?;
        }
        Ok(())
    }

    fn resize(&mut self, height: u16) -> io::Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            terminal.clear()?;
            drop(terminal);
            let options = TerminalOptions {
                viewport: Viewport::Inline(height),
            };
            let new_terminal =
                Terminal::with_options(CrosstermBackend::new(io::stdout()), options)?;
            self.terminal = Some(new_terminal);
            self.height = height;
        }
        Ok(())
    }

    /// Clears the inline viewport, disables raw mode, flushes stdout, and drops the terminal
    /// so an interactive subprocess can own the TTY.
    pub fn suspend(&mut self) -> io::Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            terminal.clear()?;
            crossterm::terminal::disable_raw_mode()?;
            io::stdout().flush()?;
            drop(terminal);
        }
        Ok(())
    }

    /// Re-enables raw mode and recreates the inline viewport after an interactive subprocess exits.
    pub fn resume(&mut self, height: u16) -> io::Result<()> {
        if self.terminal.is_none() {
            let effective_height = height.max(self.height);
            let terminal = create_inline_terminal(effective_height)?;
            self.terminal = Some(terminal);
            self.height = effective_height;
        }
        Ok(())
    }

    /// Leaves raw mode and flushes stdout while preserving the rendered tree in scrollback.
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(terminal) = self.terminal.take() {
            crossterm::terminal::disable_raw_mode()?;
            io::stdout().flush()?;
            drop(terminal);
        }
        Ok(())
    }
}

impl Drop for InlineViewport {
    fn drop(&mut self) {
        if self.terminal.is_some() {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = io::stdout().flush();
        }
    }
}

fn create_inline_terminal(height: u16) -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let options = TerminalOptions {
        viewport: Viewport::Inline(height),
    };
    match Terminal::with_options(CrosstermBackend::new(io::stdout()), options) {
        Ok(terminal) => Ok(terminal),
        Err(err) => {
            let _ = crossterm::terminal::disable_raw_mode();
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::builder::makepkg::BuildPhase;

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn draw_tree_renders_pending_building_with_log_tail_and_done_nodes() {
        let mut state = AppState::new();
        state.register_base("paru-bin", "2.0.4-1");
        state.set_phase("paru-bin", BuildPhase::Done);

        state.register_base("yay-bin", "12.4.2-1");
        state.set_phase("yay-bin", BuildPhase::Building);
        for i in 1..=7 {
            state.push_log("yay-bin", format!("gcc -O2 step {i}"));
        }

        state.register_base("aurutils", "19-1");
        // Remains Pending.

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| draw_tree(frame, frame.area(), &state))
            .expect("draw_tree should render");

        let rendered = buffer_to_string(&terminal);

        // Header progress
        assert!(
            rendered.contains("Building AUR packages [1/3]"),
            "missing header in:\n{rendered}"
        );

        // Done node
        assert!(rendered.contains("✔ paru-bin 2.0.4-1 [done]"), "{rendered}");

        // Building node with spinner and 5-line log tail (lines 3..=7)
        assert!(
            rendered.contains("⠋ yay-bin 12.4.2-1 [building]"),
            "{rendered}"
        );
        assert!(!rendered.contains("gcc -O2 step 1"), "{rendered}");
        assert!(!rendered.contains("gcc -O2 step 2"), "{rendered}");
        for i in 3..=7 {
            assert!(
                rendered.contains(&format!("│ gcc -O2 step {i}")),
                "missing log line {i} in:\n{rendered}"
            );
        }

        // Pending node
        assert!(rendered.contains("· aurutils 19-1 [pending]"), "{rendered}");
    }

    #[test]
    fn draw_tree_renders_failed_node_with_log_tail() {
        let mut state = AppState::new();
        state.register_base("broken-pkg", "0.1.0-1");
        state.set_phase("broken-pkg", BuildPhase::Building);
        state.push_log("broken-pkg", "error: compilation failed");
        state.set_phase("broken-pkg", BuildPhase::Failed);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| draw_tree(frame, frame.area(), &state))
            .expect("draw_tree should render");

        let rendered = buffer_to_string(&terminal);
        assert!(
            rendered.contains("✖ broken-pkg 0.1.0-1 [failed]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("│ error: compilation failed"),
            "{rendered}"
        );
    }

    #[test]
    fn draw_tree_renders_bottom_progress_bar_for_aur_builds() {
        let mut state = AppState::new();
        state.register_base("paru-bin", "2.0.4-1");
        state.set_phase("paru-bin", BuildPhase::Done);
        state.register_base("yay-bin", "12.4.2-1");
        state.set_phase("yay-bin", BuildPhase::Building);
        state.register_base("aurutils", "19-1");

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| draw_tree(frame, frame.area(), &state))
            .expect("draw_tree should render");

        let rendered = buffer_to_string(&terminal);
        let lines: Vec<&str> = rendered.lines().collect();
        let bottom = lines.last().copied().unwrap_or("");
        assert!(bottom.contains('━'), "missing filled bar in:\n{rendered}");
        assert!(bottom.contains('─'), "missing empty bar in:\n{rendered}");
        assert!(
            bottom.contains("1/3 (33%)"),
            "missing count in:\n{rendered}"
        );
        assert!(
            bottom.contains("building yay-bin"),
            "missing active detail in:\n{rendered}"
        );
    }

    #[test]
    fn draw_tree_renders_repo_install_progress_bar_and_node_tail() {
        let mut state = AppState::new();
        state.set_header("Installing official packages");
        for i in 1..=7 {
            state.set_repo_phase("pacman", "installing", Some(format!("({i}/6) step {i}")));
        }
        state.set_progress("Installing", 3, 6, Some("installing gedit".to_string()));

        let height = compute_viewport_height(&state);
        assert_eq!(height, 8); // 1 header + 1 node + 5 log lines + 1 progress bar

        let backend = TestBackend::new(80, height);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| draw_tree(frame, frame.area(), &state))
            .expect("draw_tree should render");

        let rendered = buffer_to_string(&terminal);
        assert!(
            rendered.contains("Installing official packages [0/1]"),
            "{rendered}"
        );
        assert!(rendered.contains("⠋ pacman [installing]"), "{rendered}");
        assert!(!rendered.contains("(2/6) step 2"), "{rendered}");
        for i in 3..=7 {
            assert!(
                rendered.contains(&format!("│ ({i}/6) step {i}")),
                "{rendered}"
            );
        }

        let bottom = rendered.lines().last().unwrap_or("");
        assert!(bottom.contains('━'), "{rendered}");
        assert!(bottom.contains('╸'), "{rendered}");
        assert!(bottom.contains('─'), "{rendered}");
        assert!(bottom.contains("3/6 (50%)"), "{rendered}");
        assert!(bottom.contains("installing gedit"), "{rendered}");
    }

    #[test]
    fn compute_viewport_height_grows_with_nodes_logs_and_progress_bar() {
        let mut state = AppState::new();
        assert_eq!(compute_viewport_height(&state), MIN_VIEWPORT_HEIGHT);

        state.set_progress("Retrieving", 0, 0, Some("syncing".to_string()));
        assert_eq!(compute_viewport_height(&state), MIN_VIEWPORT_HEIGHT);

        state.set_repo_phase("pacman", "installing", Some("line 1".into()));
        state.set_repo_phase("pacman", "installing", Some("line 2".into()));
        state.set_repo_phase("pacman", "installing", Some("line 3".into()));
        // 1 header + 1 node + 3 logs + 1 progress bar = 6
        assert_eq!(compute_viewport_height(&state), 6);
    }
}
