use std::cmp::Ordering;
use std::io::{self, BufRead, IsTerminal, Write};

use owo_colors::OwoColorize;
#[cfg(test)]
use ratatui::style::{Color, Modifier, Style};
#[cfg(test)]
use ratatui::text::{Line, Span};
#[cfg(test)]
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::alpm_ctx::PkgSummary;
use crate::aur::resolver::{ChangeKind, InstallRole, PlannedPkg, TransactionPlan};
#[cfg(test)]
use crate::aur::resolver::{ConflictEntry, PlannedAurBase};
use crate::aur::rpc::AurSearchHit;

/// Returns `true` when ANSI color output should be emitted to `stdout`.
pub fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal()
}

/// Splits `new_ver` into `(common_prefix, changed_suffix)` relative to `old_ver`
/// on character boundaries for `nh`-style version diff highlighting.
fn split_version_diff<'a>(old_ver: &str, new_ver: &'a str) -> (&'a str, &'a str) {
    let common_bytes = old_ver
        .char_indices()
        .zip(new_ver.char_indices())
        .take_while(|((_, a), (_, b))| a == b)
        .last()
        .map(|(_, (idx, ch))| idx + ch.len_utf8())
        .unwrap_or(0);
    new_ver.split_at(common_bytes)
}

/// Formats a byte count into a human-readable MiB/KiB/B string.
fn format_bytes(bytes: i64) -> String {
    let abs = bytes.unsigned_abs() as f64;
    let sign = if bytes < 0 { "-" } else { "" };
    if abs >= 1024.0 * 1024.0 {
        format!("{sign}{:.2} MiB", abs / (1024.0 * 1024.0))
    } else if abs >= 1024.0 {
        format!("{sign}{:.2} KiB", abs / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn change_marker(change: ChangeKind) -> &'static str {
    match change {
        ChangeKind::Install => "+",
        ChangeKind::Upgrade | ChangeKind::Reinstall => "~",
        ChangeKind::Downgrade => "!",
    }
}

#[cfg(test)]
fn style_if(color: bool, style: Style) -> Style {
    if color { style } else { Style::default() }
}

#[cfg(test)]
fn build_pkg_ratatui_line(pkg: &PlannedPkg, is_make_only: bool, color: bool) -> Line<'static> {
    let marker = change_marker(pkg.change);
    let marker_style = match pkg.change {
        ChangeKind::Install => style_if(
            color,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ChangeKind::Upgrade => style_if(
            color,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        ChangeKind::Reinstall => style_if(color, Style::default().fg(Color::Cyan)),
        ChangeKind::Downgrade => style_if(
            color,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let name_style = style_if(color, Style::default().add_modifier(Modifier::BOLD));
    let dim_style = style_if(color, Style::default().add_modifier(Modifier::DIM));
    let arrow_style = style_if(color, Style::default().fg(Color::Cyan));

    let mut spans = vec![
        Span::styled(format!("  {marker} "), marker_style),
        Span::styled(format!("{:<24} ", pkg.name), name_style),
    ];

    match pkg.change {
        ChangeKind::Install => {
            spans.push(Span::styled(
                format!("{} (new)", pkg.new_version),
                style_if(color, Style::default().fg(Color::Green)),
            ));
        }
        ChangeKind::Upgrade => {
            let old = pkg.old_version.as_deref().unwrap_or("?");
            let (prefix, suffix) = split_version_diff(old, &pkg.new_version);
            spans.push(Span::styled(old.to_owned(), dim_style));
            spans.push(Span::styled(" -> ", arrow_style));
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.to_owned(), dim_style));
            }
            spans.push(Span::styled(
                suffix.to_owned(),
                style_if(
                    color,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ));
        }
        ChangeKind::Reinstall => {
            spans.push(Span::styled(
                format!("{} (reinstall)", pkg.new_version),
                dim_style,
            ));
        }
        ChangeKind::Downgrade => {
            let old = pkg.old_version.as_deref().unwrap_or("?");
            spans.push(Span::styled(old.to_owned(), dim_style));
            spans.push(Span::styled(" -> ", arrow_style));
            spans.push(Span::styled(
                format!("{} (downgrade)", pkg.new_version),
                style_if(
                    color,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            ));
        }
    }

    if is_make_only || pkg.role == InstallRole::MakeDependency {
        spans.push(Span::styled(
            " [make]",
            style_if(color, Style::default().fg(Color::Magenta)),
        ));
    }

    Line::from(spans)
}

#[cfg(test)]
fn build_aur_base_lines(base: &PlannedAurBase, color: bool) -> Vec<Line<'static>> {
    base.packages
        .iter()
        .map(|pkg| build_pkg_ratatui_line(pkg, base.make_only, color))
        .collect()
}

#[cfg(test)]
fn build_conflict_line(conflict: &ConflictEntry, color: bool) -> Line<'static> {
    let bang_style = style_if(
        color,
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    );
    let reason = conflict
        .reason
        .as_deref()
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled("  ! ", bang_style),
        Span::styled(
            format!(
                "{} conflicts with {}{reason}",
                conflict.pkg, conflict.conflicting_pkg
            ),
            style_if(color, Style::default().fg(Color::Magenta)),
        ),
    ])
}

/// Renders the `nh`-style pre-transaction grouped diff table as a `ratatui` [`Paragraph`],
/// suitable for both terminal rendering and `ratatui::backend::TestBackend` unit tests.
#[cfg(test)]
pub fn render_summary_widgets(plan: &TransactionPlan) -> Paragraph<'static> {
    render_summary_widgets_with_color(plan, true)
}

/// Renders the pre-transaction diff table with explicit color control.
#[cfg(test)]
pub fn render_summary_widgets_with_color(
    plan: &TransactionPlan,
    color: bool,
) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let header_style = style_if(
        color,
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    );

    if !plan.repo_packages.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Official Repositories ({}):", plan.repo_packages.len()),
            header_style,
        )));
        for pkg in &plan.repo_packages {
            lines.push(build_pkg_ratatui_line(pkg, false, color));
        }
    }

    if !plan.aur_bases.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let aur_pkg_count: usize = plan.aur_bases.iter().map(|b| b.packages.len()).sum();
        lines.push(Line::from(Span::styled(
            format!("AUR ({aur_pkg_count}):"),
            style_if(
                color,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )));
        for base in &plan.aur_bases {
            lines.extend(build_aur_base_lines(base, color));
        }
    }

    if !plan.conflicts.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Conflicts ({}):", plan.conflicts.len()),
            style_if(
                color,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        )));
        for conflict in &plan.conflicts {
            lines.push(build_conflict_line(conflict, color));
        }
    }

    let total_dl: i64 = plan.repo_packages.iter().map(|p| p.download_size).sum();
    let total_delta: i64 = plan.repo_packages.iter().map(|p| p.net_size_delta).sum();
    if total_dl > 0 || total_delta != 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "Total Download Size: {}  |  Net Installed Size: {}",
                format_bytes(total_dl),
                format_bytes(total_delta)
            ),
            style_if(color, Style::default().add_modifier(Modifier::DIM)),
        )));
    }

    if lines.is_empty() {
        lines.push(Line::from("Nothing to do."));
    }

    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::NONE)
                .title("Transaction Summary"),
        )
        .wrap(Wrap { trim: false })
}

fn format_pkg_stdout(pkg: &PlannedPkg, is_make_only: bool, color: bool) -> String {
    let marker = change_marker(pkg.change);
    let make_tag = if is_make_only || pkg.role == InstallRole::MakeDependency {
        if color {
            format!(" {}", "[make]".magenta())
        } else {
            " [make]".to_owned()
        }
    } else {
        String::new()
    };

    if !color {
        let ver = match pkg.change {
            ChangeKind::Install => format!("{} (new)", pkg.new_version),
            ChangeKind::Upgrade => format!(
                "{} -> {}",
                pkg.old_version.as_deref().unwrap_or("?"),
                pkg.new_version
            ),
            ChangeKind::Reinstall => format!("{} (reinstall)", pkg.new_version),
            ChangeKind::Downgrade => format!(
                "{} -> {} (downgrade)",
                pkg.old_version.as_deref().unwrap_or("?"),
                pkg.new_version
            ),
        };
        return format!("  {marker} {:<24} {ver}{make_tag}", pkg.name);
    }

    let colored_marker = match pkg.change {
        ChangeKind::Install => marker.green().bold().to_string(),
        ChangeKind::Upgrade => marker.yellow().bold().to_string(),
        ChangeKind::Reinstall => marker.cyan().to_string(),
        ChangeKind::Downgrade => marker.red().bold().to_string(),
    };
    let colored_name = format!("{:<24}", pkg.name).bold().to_string();
    let ver = match pkg.change {
        ChangeKind::Install => format!("{} (new)", pkg.new_version).green().to_string(),
        ChangeKind::Upgrade => {
            let old = pkg.old_version.as_deref().unwrap_or("?");
            let (prefix, suffix) = split_version_diff(old, &pkg.new_version);
            format!(
                "{} {} {}{}",
                old.dimmed(),
                "->".cyan(),
                prefix.dimmed(),
                suffix.yellow().bold()
            )
        }
        ChangeKind::Reinstall => format!("{} (reinstall)", pkg.new_version)
            .dimmed()
            .to_string(),
        ChangeKind::Downgrade => {
            let old = pkg.old_version.as_deref().unwrap_or("?");
            format!(
                "{} {} {}",
                old.dimmed(),
                "->".cyan(),
                format!("{} (downgrade)", pkg.new_version).red().bold()
            )
        }
    };

    format!("  {colored_marker} {colored_name} {ver}{make_tag}")
}

/// Prints the `nh`-style pre-transaction summary to `stdout`, respecting `NO_COLOR` and TTY status.
pub fn print_summary(plan: &TransactionPlan) {
    let color = use_color();

    if !plan.repo_packages.is_empty() {
        let header = format!("Official Repositories ({}):", plan.repo_packages.len());
        if color {
            println!("{}", header.blue().bold());
        } else {
            println!("{header}");
        }
        for pkg in &plan.repo_packages {
            println!("{}", format_pkg_stdout(pkg, false, color));
        }
    }

    if !plan.aur_bases.is_empty() {
        if !plan.repo_packages.is_empty() {
            println!();
        }
        let aur_count: usize = plan.aur_bases.iter().map(|b| b.packages.len()).sum();
        let header = format!("AUR ({aur_count}):");
        if color {
            println!("{}", header.cyan().bold());
        } else {
            println!("{header}");
        }
        for base in &plan.aur_bases {
            for pkg in &base.packages {
                println!("{}", format_pkg_stdout(pkg, base.make_only, color));
            }
        }
    }

    if !plan.conflicts.is_empty() {
        println!();
        let header = format!("Conflicts ({}):", plan.conflicts.len());
        if color {
            println!("{}", header.red().bold());
        } else {
            println!("{header}");
        }
        for c in &plan.conflicts {
            let reason = c
                .reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            let line = format!("  ! {} conflicts with {}{reason}", c.pkg, c.conflicting_pkg);
            if color {
                println!("{}", line.magenta());
            } else {
                println!("{line}");
            }
        }
    }

    let total_dl: i64 = plan.repo_packages.iter().map(|p| p.download_size).sum();
    let total_delta: i64 = plan.repo_packages.iter().map(|p| p.net_size_delta).sum();
    if total_dl > 0 || total_delta != 0 {
        println!();
        let totals = format!(
            "Total Download Size: {}  |  Net Installed Size: {}",
            format_bytes(total_dl),
            format_bytes(total_delta)
        );
        if color {
            println!("{}", totals.dimmed());
        } else {
            println!("{totals}");
        }
    }
}

/// Package metadata displayed in the `nh`-style removal summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalPkg {
    pub name: String,
    pub version: String,
    pub installed_size: i64,
}

#[cfg(test)]
fn build_removal_ratatui_line(pkg: &RemovalPkg, color: bool) -> Line<'static> {
    let minus_style = style_if(color, Style::default().fg(Color::Red));
    let name_style = style_if(color, Style::default().add_modifier(Modifier::BOLD));
    let remove_style = style_if(color, Style::default().fg(Color::Red));

    Line::from(vec![
        Span::styled("  - ", minus_style),
        Span::styled(format!("{:<24} ", pkg.name), name_style),
        Span::raw(format!("{} ", pkg.version)),
        Span::styled("(remove)", remove_style),
    ])
}

/// Renders the `nh`-style removal summary as `ratatui` [`Line`]s for `TestBackend` tests.
#[cfg(test)]
pub fn render_removal_summary_lines(pkgs: &[RemovalPkg], color: bool) -> Vec<Line<'static>> {
    if pkgs.is_empty() {
        return vec![Line::from("Nothing to do.")];
    }

    let mut lines = Vec::with_capacity(pkgs.len() + 3);
    lines.push(Line::from(Span::styled(
        format!("Removals ({}):", pkgs.len()),
        style_if(
            color,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    )));
    for pkg in pkgs {
        lines.push(build_removal_ratatui_line(pkg, color));
    }

    let total_removed: i64 = pkgs.iter().map(|p| p.installed_size).sum();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("Total Removed Size: {}", format_bytes(total_removed)),
        style_if(color, Style::default().add_modifier(Modifier::DIM)),
    )));
    lines
}

fn write_removal_summary(out: &mut impl Write, pkgs: &[RemovalPkg], color: bool) -> io::Result<()> {
    if pkgs.is_empty() {
        return Ok(());
    }

    let header = format!("Removals ({}):", pkgs.len());
    if color {
        writeln!(out, "{}", header.red().bold())?;
    } else {
        writeln!(out, "{header}")?;
    }

    for pkg in pkgs {
        if color {
            let colored_name = format!("{:<24}", pkg.name).bold().to_string();
            writeln!(
                out,
                "  {} {} {} {}",
                "-".red(),
                colored_name,
                pkg.version,
                "(remove)".red()
            )?;
        } else {
            writeln!(out, "  - {:<24} {} (remove)", pkg.name, pkg.version)?;
        }
    }

    let total_removed: i64 = pkgs.iter().map(|p| p.installed_size).sum();
    writeln!(out)?;
    let footer = format!("Total Removed Size: {}", format_bytes(total_removed));
    if color {
        writeln!(out, "{}", footer.dimmed())?;
    } else {
        writeln!(out, "{footer}")?;
    }

    Ok(())
}

/// Prints the `nh`-style removal summary to `stdout`, respecting `NO_COLOR` and TTY status.
pub fn print_removal_summary(pkgs: &[RemovalPkg]) {
    let _ = write_removal_summary(&mut io::stdout(), pkgs, use_color());
}

/// Prompts the user for `[Y/n]` or `[y/N]` confirmation on `stdin`/`stdout`.
///
/// Returns `Ok(default_yes)` immediately when `noconfirm` is `true`.
pub fn confirm_prompt(prompt: &str, default_yes: bool, noconfirm: bool) -> io::Result<bool> {
    if noconfirm {
        return Ok(default_yes);
    }

    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {suffix} ");
    io::stdout().flush()?;

    let mut line = String::new();
    let bytes = io::stdin().lock().read_line(&mut line)?;
    if bytes == 0 {
        return Ok(false);
    }

    let trimmed = line.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        Ok(default_yes)
    } else if matches!(trimmed.as_str(), "y" | "yes") {
        Ok(true)
    } else if matches!(trimmed.as_str(), "n" | "no") {
        Ok(false)
    } else {
        Ok(default_yes)
    }
}

/// Formats and prints `-Ss` search results to `stdout` in bottom-up order
/// (AUR sorted ascending by popularity first, then official repos in reverse order).
pub fn print_search_results(repo_hits: &[PkgSummary], aur_hits: &[AurSearchHit], quiet: bool) {
    let _ = write_search_results(&mut io::stdout(), repo_hits, aur_hits, quiet, use_color());
}

fn write_search_results(
    out: &mut impl Write,
    repo_hits: &[PkgSummary],
    aur_hits: &[AurSearchHit],
    quiet: bool,
    color: bool,
) -> io::Result<()> {
    let mut sorted_aur: Vec<&AurSearchHit> = aur_hits.iter().collect();
    sorted_aur.sort_by(|a, b| {
        a.popularity
            .partial_cmp(&b.popularity)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.votes.cmp(&b.votes))
            .then_with(|| b.name.cmp(&a.name))
    });

    for hit in sorted_aur {
        if quiet {
            writeln!(out, "aur/{}", hit.name)?;
            continue;
        }
        let ood_badge = if hit.out_of_date.is_some() {
            " [out-of-date]"
        } else {
            ""
        };
        let installed_badge = match &hit.installed_version {
            Some(v) if v == &hit.version => " [installed]".to_owned(),
            Some(v) => format!(" [installed: {v}]"),
            None => String::new(),
        };
        let stats = format!("(+{} {:.2})", hit.votes, hit.popularity);
        if color {
            writeln!(
                out,
                "{}/{} {} {}{}{}",
                "aur".blue().bold(),
                hit.name.bold(),
                hit.version.green(),
                stats.dimmed(),
                ood_badge.red(),
                installed_badge.cyan()
            )?;
        } else {
            writeln!(
                out,
                "aur/{} {} {}{}{}",
                hit.name, hit.version, stats, ood_badge, installed_badge
            )?;
        }
        if let Some(desc) = &hit.description
            && !desc.is_empty()
        {
            writeln!(out, "    {desc}")?;
        }
    }

    for pkg in repo_hits.iter().rev() {
        if quiet {
            writeln!(out, "{}/{}", pkg.repo, pkg.name)?;
            continue;
        }
        let installed_badge = match &pkg.installed_version {
            Some(v) if v == &pkg.version => " [installed]".to_owned(),
            Some(v) => format!(" [installed: {v}]"),
            None => String::new(),
        };
        if color {
            writeln!(
                out,
                "{}/{} {}{}",
                pkg.repo.magenta().bold(),
                pkg.name.bold(),
                pkg.version.green(),
                installed_badge.cyan()
            )?;
        } else {
            writeln!(
                out,
                "{}/{} {}{}",
                pkg.repo, pkg.name, pkg.version, installed_badge
            )?;
        }
        if !pkg.description.is_empty() {
            writeln!(out, "    {}", pkg.description)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_plan() -> TransactionPlan {
        TransactionPlan {
            repo_packages: vec![
                PlannedPkg {
                    name: "linux".to_owned(),
                    source_repo: "core".to_owned(),
                    old_version: Some("6.11.1-1".to_owned()),
                    new_version: "6.12.0-1".to_owned(),
                    change: ChangeKind::Upgrade,
                    role: InstallRole::ExplicitTarget,
                    download_size: 120 * 1024 * 1024,
                    net_size_delta: 2 * 1024 * 1024,
                },
                PlannedPkg {
                    name: "ripgrep".to_owned(),
                    source_repo: "extra".to_owned(),
                    old_version: None,
                    new_version: "14.1.1-1".to_owned(),
                    change: ChangeKind::Install,
                    role: InstallRole::Dependency,
                    download_size: 2 * 1024 * 1024,
                    net_size_delta: 5 * 1024 * 1024,
                },
                PlannedPkg {
                    name: "mesa".to_owned(),
                    source_repo: "extra".to_owned(),
                    old_version: Some("24.3.0-1".to_owned()),
                    new_version: "24.2.1-1".to_owned(),
                    change: ChangeKind::Downgrade,
                    role: InstallRole::ExplicitTarget,
                    download_size: 1024,
                    net_size_delta: -512,
                },
            ],
            aur_bases: vec![
                PlannedAurBase {
                    pkgbase: "cargo-audit".to_owned(),
                    version: "0.21.0-1".to_owned(),
                    packages: vec![PlannedPkg {
                        name: "cargo-audit".to_owned(),
                        source_repo: "aur".to_owned(),
                        old_version: None,
                        new_version: "0.21.0-1".to_owned(),
                        change: ChangeKind::Install,
                        role: InstallRole::MakeDependency,
                        download_size: 0,
                        net_size_delta: 0,
                    }],
                    make_only: true,
                    is_explicit_target: false,
                },
                PlannedAurBase {
                    pkgbase: "paru-bin".to_owned(),
                    version: "2.0.4-1".to_owned(),
                    packages: vec![PlannedPkg {
                        name: "paru-bin".to_owned(),
                        source_repo: "aur".to_owned(),
                        old_version: Some("2.0.3-1".to_owned()),
                        new_version: "2.0.4-1".to_owned(),
                        change: ChangeKind::Upgrade,
                        role: InstallRole::ExplicitTarget,
                        download_size: 0,
                        net_size_delta: 0,
                    }],
                    make_only: false,
                    is_explicit_target: true,
                },
            ],
            unneeded: Vec::new(),
            missing: Vec::new(),
            conflicts: vec![ConflictEntry {
                pkg: "paru-bin".to_owned(),
                conflicting_pkg: "paru".to_owned(),
                reason: Some("provides paru".to_owned()),
            }],
        }
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_grouped_summary_with_test_backend() {
        let plan = sample_plan();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let widget = render_summary_widgets(&plan);
                frame.render_widget(widget, frame.area());
            })
            .expect("draw summary");

        let rendered = buffer_to_string(&terminal);
        assert!(rendered.contains("Official Repositories (3):"));
        assert!(rendered.contains("~ linux"));
        assert!(rendered.contains("6.11.1-1 -> 6.12.0-1"));
        assert!(rendered.contains("+ ripgrep"));
        assert!(rendered.contains("14.1.1-1 (new)"));
        assert!(rendered.contains("! mesa"));
        assert!(rendered.contains("24.2.1-1 (downgrade)"));
        assert!(rendered.contains("AUR (2):"));
        assert!(rendered.contains("+ cargo-audit"));
        assert!(rendered.contains("[make]"));
        assert!(rendered.contains("~ paru-bin"));
        assert!(rendered.contains("2.0.3-1 -> 2.0.4-1"));
        assert!(rendered.contains("Conflicts (1):"));
        assert!(rendered.contains("paru-bin conflicts with paru (provides paru)"));
    }

    #[test]
    fn confirm_prompt_respects_noconfirm() {
        assert!(confirm_prompt("Proceed?", true, true).unwrap());
        assert!(!confirm_prompt("Proceed?", false, true).unwrap());
    }

    #[test]
    fn split_version_diff_handles_utf8_and_common_prefix() {
        assert_eq!(split_version_diff("2.0.3-1", "2.0.4-1"), ("2.0.", "4-1"));
        assert_eq!(split_version_diff("1.0-1", "2.0-1"), ("", "2.0-1"));
    }

    #[test]
    fn search_results_print_bottom_up_aur_then_reversed_repos() {
        let repo_hits = vec![
            PkgSummary {
                repo: "core".to_owned(),
                name: "pacman".to_owned(),
                version: "7.0.0-1".to_owned(),
                description: "A library-based package manager".to_owned(),
                download_size: 1000,
                installed_size: 2000,
                installed_version: Some("7.0.0-1".to_owned()),
            },
            PkgSummary {
                repo: "extra".to_owned(),
                name: "pacman-contrib".to_owned(),
                version: "1.10.6-1".to_owned(),
                description: "Contributed scripts and tools for pacman".to_owned(),
                download_size: 500,
                installed_size: 1000,
                installed_version: None,
            },
        ];

        let aur_hits = vec![
            AurSearchHit {
                name: "paru".to_owned(),
                version: "2.0.4-1".to_owned(),
                description: Some("Feature packed AUR helper".to_owned()),
                votes: 900,
                popularity: 25.5,
                maintainer: Some("morganamilo".to_owned()),
                out_of_date: None,
                installed_version: None,
            },
            AurSearchHit {
                name: "alpha-helper".to_owned(),
                version: "1.0.0-1".to_owned(),
                description: Some("Alpha helper".to_owned()),
                votes: 10,
                popularity: 1.5,
                maintainer: None,
                out_of_date: None,
                installed_version: None,
            },
            AurSearchHit {
                name: "beta-helper".to_owned(),
                version: "1.0.0-1".to_owned(),
                description: Some("Beta helper".to_owned()),
                votes: 10,
                popularity: 1.5,
                maintainer: None,
                out_of_date: None,
                installed_version: None,
            },
            AurSearchHit {
                name: "mid-helper".to_owned(),
                version: "1.2.0-1".to_owned(),
                description: Some("Mid helper".to_owned()),
                votes: 20,
                popularity: 1.5,
                maintainer: None,
                out_of_date: None,
                installed_version: None,
            },
        ];

        let mut quiet_out = Vec::new();
        write_search_results(&mut quiet_out, &repo_hits, &aur_hits, true, false).unwrap();
        let quiet_lines: Vec<&str> = std::str::from_utf8(&quiet_out).unwrap().lines().collect();
        assert_eq!(
            quiet_lines,
            vec![
                "aur/beta-helper",
                "aur/alpha-helper",
                "aur/mid-helper",
                "aur/paru",
                "extra/pacman-contrib",
                "core/pacman",
            ]
        );

        let mut full_out = Vec::new();
        write_search_results(&mut full_out, &repo_hits, &aur_hits, false, false).unwrap();
        let full_str = String::from_utf8(full_out).unwrap();
        let beta_pos = full_str.find("aur/beta-helper 1.0.0-1 (+10 1.50)").unwrap();
        let alpha_pos = full_str
            .find("aur/alpha-helper 1.0.0-1 (+10 1.50)")
            .unwrap();
        let mid_pos = full_str.find("aur/mid-helper 1.2.0-1 (+20 1.50)").unwrap();
        let paru_pos = full_str.find("aur/paru 2.0.4-1 (+900 25.50)").unwrap();
        let extra_pos = full_str.find("extra/pacman-contrib 1.10.6-1").unwrap();
        let core_pos = full_str.find("core/pacman 7.0.0-1 [installed]").unwrap();
        assert!(beta_pos < alpha_pos);
        assert!(alpha_pos < mid_pos);
        assert!(mid_pos < paru_pos);
        assert!(paru_pos < extra_pos);
        assert!(extra_pos < core_pos);
    }

    #[test]
    fn renders_removal_summary_with_test_backend() {
        let pkgs = vec![
            RemovalPkg {
                name: "ripgrep".to_owned(),
                version: "14.1.1-1".to_owned(),
                installed_size: 5 * 1024 * 1024,
            },
            RemovalPkg {
                name: "fd".to_owned(),
                version: "10.2.0-1".to_owned(),
                installed_size: 2 * 1024 * 1024,
            },
        ];

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let lines = render_removal_summary_lines(&pkgs, true);
                let widget = Paragraph::new(lines).wrap(Wrap { trim: false });
                frame.render_widget(widget, frame.area());
            })
            .expect("draw removal summary");

        let rendered = buffer_to_string(&terminal);
        assert!(rendered.contains("Removals (2):"));
        assert!(rendered.contains("- ripgrep"));
        assert!(rendered.contains("14.1.1-1 (remove)"));
        assert!(rendered.contains("- fd"));
        assert!(rendered.contains("10.2.0-1 (remove)"));
        assert!(rendered.contains("Total Removed Size: 7.00 MiB"));
    }

    #[test]
    fn write_removal_summary_plain_and_colored_and_empty() {
        let mut empty_out = Vec::new();
        write_removal_summary(&mut empty_out, &[], false).unwrap();
        assert!(empty_out.is_empty());

        let pkgs = vec![RemovalPkg {
            name: "bat".to_owned(),
            version: "0.24.0-1".to_owned(),
            installed_size: 3 * 1024 * 1024,
        }];

        let mut plain_out = Vec::new();
        write_removal_summary(&mut plain_out, &pkgs, false).unwrap();
        let plain = String::from_utf8(plain_out).unwrap();
        assert!(plain.contains("Removals (1):"));
        assert!(plain.contains("  - bat                      0.24.0-1 (remove)"));
        assert!(plain.contains("Total Removed Size: 3.00 MiB"));
        assert!(!plain.contains('\x1b'));

        let mut color_out = Vec::new();
        write_removal_summary(&mut color_out, &pkgs, true).unwrap();
        let colored = String::from_utf8(color_out).unwrap();
        assert!(colored.contains('\x1b'));
        assert!(colored.contains("Removals (1):"));
        assert!(colored.contains("bat"));
        assert!(colored.contains("0.24.0-1"));
        assert!(colored.contains("(remove)"));
        assert!(colored.contains("Total Removed Size: 3.00 MiB"));
    }
}
