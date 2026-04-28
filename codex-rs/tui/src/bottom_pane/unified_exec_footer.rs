//! Renders and formats unified-exec background session summary text.
//!
//! This module provides one canonical summary string so the bottom pane can
//! either render a dedicated footer row or reuse the same text inline in the
//! status row without duplicating copy/grammar logic.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use codex_protocol::protocol::ExecCommandRunMode;
use std::time::Instant;

use crate::live_wrap::take_prefix_by_width;
use crate::render::renderable::Renderable;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandActivity {
    pub(crate) command: String,
    pub(crate) shell_id: String,
    pub(crate) run_mode: Option<ExecCommandRunMode>,
    pub(crate) started_at: Instant,
    pub(crate) status: String,
    pub(crate) recent_output: Vec<String>,
}

/// Tracks active unified-exec processes and renders a compact summary.
pub(crate) struct UnifiedExecFooter {
    processes: Vec<CommandActivity>,
    subagents: Vec<String>,
    focused: bool,
}

impl UnifiedExecFooter {
    pub(crate) fn new() -> Self {
        Self {
            processes: Vec::new(),
            subagents: Vec::new(),
            focused: false,
        }
    }

    pub(crate) fn set_processes(&mut self, processes: Vec<CommandActivity>) -> bool {
        if self.processes == processes {
            return false;
        }
        self.processes = processes;
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.processes.is_empty() && self.subagents.is_empty()
    }

    pub(crate) fn set_subagents(&mut self, subagents: Vec<String>) -> bool {
        if self.subagents == subagents {
            return false;
        }
        self.subagents = subagents;
        true
    }

    pub(crate) fn set_focused(&mut self, focused: bool) -> bool {
        if self.focused == focused {
            return false;
        }
        self.focused = focused;
        true
    }

    pub(crate) fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let blocking = self.blocking_processes();
        if !blocking.is_empty() {
            lines.push("Commands".to_string());
            append_process_details(&mut lines, blocking);
        }
        let background = self.background_processes();
        if !background.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("Background commands".to_string());
            append_process_details(&mut lines, background);
        }
        if !self.subagents.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("Subagents".to_string());
            lines.extend(self.subagents.iter().map(|agent| format!("  {agent}")));
        }
        lines
    }

    fn blocking_processes(&self) -> Vec<&CommandActivity> {
        self.processes
            .iter()
            .filter(|process| !matches!(process.run_mode, Some(ExecCommandRunMode::Background)))
            .collect()
    }

    fn background_processes(&self) -> Vec<&CommandActivity> {
        self.processes
            .iter()
            .filter(|process| matches!(process.run_mode, Some(ExecCommandRunMode::Background)))
            .collect()
    }

    fn summary_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        let blocking_count = self.blocking_processes().len();
        if blocking_count > 0 {
            let count = blocking_count;
            let plural = if count == 1 { "" } else { "s" };
            rows.push(format!("{count} command{plural} running"));
        }
        let background_count = self.background_processes().len();
        if background_count > 0 {
            let count = background_count;
            let plural = if count == 1 { "" } else { "s" };
            rows.push(format!("{count} background command{plural} running"));
        }
        if !self.subagents.is_empty() {
            let count = self.subagents.len();
            let plural = if count == 1 { "" } else { "s" };
            rows.push(format!("{count} subagent{plural} running"));
        }
        rows
    }

    fn render_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width < 4 {
            return Vec::new();
        }
        self.summary_rows()
            .into_iter()
            .map(|summary| {
                let prefix = if self.focused { "> " } else { "  " };
                let message = format!("{prefix}{summary} · Enter details · k kill");
                let (truncated, _, _) = take_prefix_by_width(&message, width as usize);
                if self.focused {
                    Line::from(truncated.cyan())
                } else {
                    Line::from(truncated.dim())
                }
            })
            .collect()
    }
}

fn append_process_details(lines: &mut Vec<String>, processes: Vec<&CommandActivity>) {
    for process in processes {
        lines.push(format!("  {}", process.command));
        lines.push(format!("    shell id: {}", process.shell_id));
        lines.push(format!("    status: {}", process.status));
        lines.push(format!(
            "    runtime: {}",
            format_duration(process.started_at.elapsed())
        ));
        if !process.recent_output.is_empty() {
            lines.push("    output:".to_string());
            lines.extend(format_output_lines(&process.recent_output));
        }
    }
}

fn format_output_lines(output: &[String]) -> Vec<String> {
    const HEAD_LINES: usize = 2;
    const TAIL_LINES: usize = 5;
    const MAX_LINES_WITHOUT_OMISSION: usize = HEAD_LINES + TAIL_LINES;

    if output.len() <= MAX_LINES_WITHOUT_OMISSION {
        return output.iter().map(|line| format!("      {line}")).collect();
    }

    let omitted = output.len() - MAX_LINES_WITHOUT_OMISSION;
    let mut lines = Vec::with_capacity(MAX_LINES_WITHOUT_OMISSION + 1);
    lines.extend(
        output
            .iter()
            .take(HEAD_LINES)
            .map(|line| format!("      {line}")),
    );
    lines.push(format!("      ... +{omitted} lines"));
    lines.extend(
        output
            .iter()
            .skip(output.len() - TAIL_LINES)
            .map(|line| format!("      {line}")),
    );
    lines
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds}s");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes}m {seconds}s")
}

impl Renderable for UnifiedExecFooter {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Paragraph::new(self.render_lines(area.width)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.render_lines(width).len() as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;

    #[test]
    fn desired_height_empty() {
        let footer = UnifiedExecFooter::new();
        assert_eq!(footer.desired_height(/*width*/ 40), 0);
    }

    #[test]
    fn render_more_sessions() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec![CommandActivity {
            command: "rg \"foo\" src".to_string(),
            shell_id: "1000".to_string(),
            run_mode: Some(ExecCommandRunMode::Blocking),
            started_at: Instant::now(),
            status: "running".to_string(),
            recent_output: Vec::new(),
        }]);
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_more_sessions", format!("{buf:?}"));
    }

    #[test]
    fn render_many_sessions() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(
            (0..123)
                .map(|idx| CommandActivity {
                    command: format!("cmd {idx}"),
                    shell_id: format!("{}", 1000 + idx),
                    run_mode: Some(ExecCommandRunMode::Background),
                    started_at: Instant::now(),
                    status: "running".to_string(),
                    recent_output: Vec::new(),
                })
                .collect(),
        );
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_many_sessions", format!("{buf:?}"));
    }

    #[test]
    fn render_commands_and_subagents() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec![CommandActivity {
            command: "cargo test".to_string(),
            shell_id: "1000".to_string(),
            run_mode: Some(ExecCommandRunMode::Blocking),
            started_at: Instant::now(),
            status: "running".to_string(),
            recent_output: vec!["running 12 tests".to_string()],
        }]);
        footer.set_subagents(vec!["explorer".to_string(), "worker".to_string()]);
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_commands_and_subagents", format!("{buf:?}"));
    }

    #[test]
    fn detail_lines_include_command_runtime_and_status() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec![CommandActivity {
            command: "cargo test".to_string(),
            shell_id: "1000".to_string(),
            run_mode: Some(ExecCommandRunMode::Background),
            started_at: Instant::now(),
            status: "running".to_string(),
            recent_output: Vec::new(),
        }]);

        let detail = footer.detail_lines().join("\n");

        assert!(detail.contains("cargo test"));
        assert!(detail.contains("shell id: 1000"));
        assert!(detail.contains("status: running"));
        assert!(detail.contains("runtime:"));
        assert!(!detail.contains("output:"));
    }
}
