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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundSubagentActivity {
    pub(crate) label: String,
    pub(crate) thread_id: String,
    pub(crate) status: String,
    pub(crate) started_at: Instant,
    pub(crate) recent_output: Vec<String>,
}

/// Tracks active unified-exec processes and renders a compact summary.
pub(crate) struct UnifiedExecFooter {
    processes: Vec<CommandActivity>,
    subagents: Vec<BackgroundSubagentActivity>,
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

    pub(crate) fn set_subagents(&mut self, subagents: Vec<BackgroundSubagentActivity>) -> bool {
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

    pub(crate) fn activity_count(&self) -> usize {
        self.blocking_processes().len() + self.background_processes().len() + self.subagents.len()
    }

    pub(crate) fn list_lines(&self, selected: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut index = 0;
        append_process_list(
            &mut lines,
            "Commands",
            self.blocking_processes(),
            &mut index,
            selected,
        );
        append_process_list(
            &mut lines,
            "Background commands",
            self.background_processes(),
            &mut index,
            selected,
        );
        if !self.subagents.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("Subagents".to_string());
            for subagent in &self.subagents {
                let prefix = if index == selected { ">" } else { " " };
                lines.push(format!(
                    "  {prefix} {} [{}]({}) · Enter details · k kill",
                    subagent.label, subagent.thread_id, subagent.status
                ));
                index += 1;
            }
        }
        lines
    }

    pub(crate) fn detail_lines(&self, selected: usize) -> Vec<String> {
        let mut index = 0;
        for process in self.blocking_processes() {
            if index == selected {
                return process_detail_lines(process);
            }
            index += 1;
        }
        for process in self.background_processes() {
            if index == selected {
                return process_detail_lines(process);
            }
            index += 1;
        }
        for subagent in &self.subagents {
            if index == selected {
                return subagent_detail_lines(subagent);
            }
            index += 1;
        }
        Vec::new()
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

fn append_process_list(
    lines: &mut Vec<String>,
    title: &str,
    processes: Vec<&CommandActivity>,
    index: &mut usize,
    selected: usize,
) {
    if processes.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(title.to_string());
    for process in processes {
        let prefix = if *index == selected { ">" } else { " " };
        lines.push(format!(
            "  {prefix} {} [{}]({}) · Enter details · k kill",
            process.command, process.shell_id, process.status
        ));
        *index += 1;
    }
}

fn process_detail_lines(process: &CommandActivity) -> Vec<String> {
    let mut lines = vec![
        process.command.clone(),
        format!("  shell id: {}", process.shell_id),
        format!("  status: {}", process.status),
        format!(
            "  runtime: {}",
            format_duration(process.started_at.elapsed())
        ),
    ];
    if !process.recent_output.is_empty() {
        lines.push("  output:".to_string());
        lines.extend(format_output_lines(&process.recent_output));
    }
    lines
}

fn subagent_detail_lines(subagent: &BackgroundSubagentActivity) -> Vec<String> {
    let mut lines = vec![
        subagent.label.clone(),
        format!("  thread id: {}", subagent.thread_id),
        format!("  status: {}", subagent.status),
        format!(
            "  runtime: {}",
            format_duration(subagent.started_at.elapsed())
        ),
    ];
    if !subagent.recent_output.is_empty() {
        lines.push("  output:".to_string());
        lines.extend(format_output_lines(&subagent.recent_output));
    }
    lines
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
        footer.set_subagents(vec![
            BackgroundSubagentActivity {
                label: "explorer".to_string(),
                thread_id: "agent-1".to_string(),
                status: "running".to_string(),
                started_at: Instant::now(),
                recent_output: Vec::new(),
            },
            BackgroundSubagentActivity {
                label: "worker".to_string(),
                thread_id: "agent-2".to_string(),
                status: "running".to_string(),
                started_at: Instant::now(),
                recent_output: Vec::new(),
            },
        ]);
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

        let detail = footer.detail_lines(0).join("\n");

        assert!(detail.contains("cargo test"));
        assert!(detail.contains("shell id: 1000"));
        assert!(detail.contains("status: running"));
        assert!(detail.contains("runtime:"));
        assert!(!detail.contains("output:"));
    }

    #[test]
    fn detail_lines_include_background_command_output() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec![CommandActivity {
            command: "npm run dev".to_string(),
            shell_id: "1000".to_string(),
            run_mode: Some(ExecCommandRunMode::Background),
            started_at: Instant::now(),
            status: "running".to_string(),
            recent_output: vec![
                "ready on http://localhost:3000".to_string(),
                "compiled successfully".to_string(),
            ],
        }]);

        let detail = footer.detail_lines(0).join("\n");

        assert!(detail.contains("npm run dev"));
        assert!(detail.contains("shell id: 1000"));
        assert!(detail.contains("status: running"));
        assert!(detail.contains("output:"));
        assert!(detail.contains("ready on http://localhost:3000"));
        assert!(detail.contains("compiled successfully"));
    }

    #[test]
    fn list_lines_include_selectable_commands_and_subagents() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_processes(vec![CommandActivity {
            command: "cargo test".to_string(),
            shell_id: "1000".to_string(),
            run_mode: Some(ExecCommandRunMode::Background),
            started_at: Instant::now(),
            status: "running".to_string(),
            recent_output: Vec::new(),
        }]);
        footer.set_subagents(vec![BackgroundSubagentActivity {
            label: "explorer".to_string(),
            thread_id: "agent-1".to_string(),
            status: "running".to_string(),
            started_at: Instant::now(),
            recent_output: Vec::new(),
        }]);

        assert_eq!(
            footer.list_lines(1),
            vec![
                "Background commands".to_string(),
                "    cargo test [1000](running) · Enter details · k kill".to_string(),
                String::new(),
                "Subagents".to_string(),
                "  > explorer [agent-1](running) · Enter details · k kill".to_string(),
            ]
        );
        assert_eq!(
            footer.detail_lines(1),
            vec![
                "explorer".to_string(),
                "  thread id: agent-1".to_string(),
                "  status: running".to_string(),
                "  runtime: 0s".to_string(),
            ]
        );
    }
}
