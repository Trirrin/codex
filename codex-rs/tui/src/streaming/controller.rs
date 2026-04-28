//! Streams markdown deltas while retaining source for later transcript reflow.
//!
//! Streaming has two outputs with different lifetimes. The live viewport needs incremental
//! `HistoryCell`s so the user sees progress, while finalized transcript history needs raw markdown
//! source so it can be rendered again after a terminal resize. These controllers keep those outputs
//! tied together: newline-complete source is rendered into queued live cells, and finalization
//! returns the accumulated source to the app for consolidation.
//!
//! Width changes are handled by re-rendering from source and rebuilding only the not-yet-emitted
//! queue. Already emitted rows stay emitted until the app-level transcript reflow rebuilds the full
//! scrollback from finalized cells.

use crate::history_cell::HistoryCell;
use crate::history_cell::{self};
use crate::markdown::append_markdown;
use crate::markdown_render::render_markdown_text_with_width_and_cwd;
use crate::render::line_utils::line_to_static;
use crate::render::line_utils::prefix_lines;
use crate::style::proposed_plan_style;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use pulldown_cmark::Alignment;
use ratatui::prelude::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

use super::StreamState;

/// Shared source-retaining stream state for assistant and plan output.
///
/// `raw_source` is the markdown source that has crossed a newline boundary and can be rendered
/// deterministically. `rendered_lines` is the current-width render of that source. `enqueued_len`
/// tracks how much of that render has been offered to the commit queue, while `emitted_len` tracks
/// how much has actually reached history cells. Keeping those counters separate lets width changes
/// rebuild pending output without duplicating lines that are already visible.
struct StreamCore {
    state: StreamState,
    width: Option<usize>,
    raw_source: String,
    rendered_lines: Vec<Line<'static>>,
    enqueued_len: usize,
    emitted_len: usize,
    cwd: PathBuf,
    live_table: Option<LivePipeTable>,
    live_table_candidate_header: Option<String>,
}

struct LivePipeTable {
    widths: Vec<usize>,
    alignments: Vec<Alignment>,
    has_rows: bool,
}

impl StreamCore {
    fn new(width: Option<usize>, cwd: &Path) -> Self {
        Self {
            state: StreamState::new(width, cwd),
            width,
            raw_source: String::with_capacity(1024),
            rendered_lines: Vec::with_capacity(64),
            enqueued_len: 0,
            emitted_len: 0,
            cwd: cwd.to_path_buf(),
            live_table: None,
            live_table_candidate_header: None,
        }
    }

    fn push_delta(&mut self, delta: &str) -> bool {
        if !delta.is_empty() {
            self.state.has_seen_delta = true;
        }
        self.state.collector.push_delta(delta);

        if delta.contains('\n')
            && let Some(committed_source) = self
                .state
                .collector
                .commit_complete_source_for_live_preview()
        {
            self.raw_source.push_str(&committed_source);
            if self.process_live_table_preview_source(&committed_source) {
                return self.state.queued_len() > 0;
            }
            self.recompute_render();
            return self.sync_queue_to_render();
        }

        false
    }

    fn finalize_remaining(&mut self) -> Vec<Line<'static>> {
        let remainder_source = self.state.collector.finalize_and_drain_source();
        if !remainder_source.is_empty() {
            self.raw_source.push_str(&remainder_source);
            if self.process_live_table_preview_source(&remainder_source) {
                self.close_live_table_preview();
                return self.rendered_lines[self.emitted_len..].to_vec();
            }
        }

        if self.live_table.is_some() {
            self.close_live_table_preview();
            return self.rendered_lines[self.emitted_len..].to_vec();
        }

        if self.live_table_candidate_header.is_some() {
            self.flush_live_table_candidate_header();
            return self.rendered_lines[self.emitted_len..].to_vec();
        }

        let mut rendered = Vec::new();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut rendered,
        );
        if self.emitted_len >= rendered.len() {
            Vec::new()
        } else {
            rendered[self.emitted_len..].to_vec()
        }
    }

    fn close_live_table_preview(&mut self) {
        if let Some(table) = self.live_table.take() {
            self.rendered_lines
                .push(live_table_border(&table.widths, '└', '┴', '┘'));
            self.enqueued_len = self.rendered_lines.len();
        }
    }

    fn flush_live_table_candidate_header(&mut self) {
        if let Some(header) = self.live_table_candidate_header.take() {
            append_markdown(
                &header,
                self.width,
                Some(self.cwd.as_path()),
                &mut self.rendered_lines,
            );
            self.enqueued_len = self.rendered_lines.len();
        }
    }

    fn tick(&mut self) -> Vec<Line<'static>> {
        let step = self.state.step();
        self.emitted_len += step.len();
        step
    }

    fn tick_batch(&mut self, max_lines: usize) -> Vec<Line<'static>> {
        if max_lines == 0 {
            return Vec::new();
        }
        let step = self.state.drain_n(max_lines);
        self.emitted_len += step.len();
        step
    }

    fn queued_lines(&self) -> usize {
        self.state.queued_len()
    }

    fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.state.oldest_queued_age(now)
    }

    fn is_idle(&self) -> bool {
        self.state.is_idle()
    }

    fn set_width(&mut self, width: Option<usize>) {
        if self.width == width {
            return;
        }

        let had_pending_queue = self.state.queued_len() > 0;
        self.width = width;
        self.state.collector.set_width(width);
        if self.raw_source.is_empty() {
            return;
        }

        self.recompute_render();
        self.emitted_len = self.emitted_len.min(self.rendered_lines.len());
        if had_pending_queue
            && self.emitted_len == self.rendered_lines.len()
            && self.emitted_len > 0
        {
            // If wrapped remainder compresses into fewer lines at the new width,
            // keep at least one line un-emitted so pre-resize pending content is
            // not skipped permanently.
            self.emitted_len -= 1;
        }

        self.state.clear_queue();
        if self.emitted_len > 0 && !had_pending_queue {
            self.enqueued_len = self.rendered_lines.len();
            return;
        }
        self.rebuild_queue_from_render();
    }

    fn clear_queue(&mut self) {
        self.state.clear_queue();
        self.enqueued_len = self.emitted_len;
    }

    fn reset(&mut self) {
        self.state.clear();
        self.raw_source.clear();
        self.rendered_lines.clear();
        self.enqueued_len = 0;
        self.emitted_len = 0;
        self.live_table = None;
        self.live_table_candidate_header = None;
    }

    fn recompute_render(&mut self) {
        self.rendered_lines.clear();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut self.rendered_lines,
        );
    }

    /// Append newly rendered lines to the live queue without replaying already queued rows.
    ///
    /// Width changes can make the rendered line count smaller than the previous queue boundary; in
    /// that case the only safe option is rebuilding the queue from `emitted_len`, because slicing
    /// from the stale `enqueued_len` would skip pending source.
    fn sync_queue_to_render(&mut self) -> bool {
        let target_len = self.rendered_lines.len().max(self.emitted_len);
        if target_len < self.enqueued_len {
            self.rebuild_queue_from_render();
            return self.state.queued_len() > 0;
        }

        if target_len == self.enqueued_len {
            return false;
        }

        self.state
            .enqueue(self.rendered_lines[self.enqueued_len..target_len].to_vec());
        self.enqueued_len = target_len;
        true
    }

    /// Rebuild the pending live queue from the current render and current emitted position.
    ///
    /// This is used when resize invalidates queued wrapping. It must never enqueue rows before
    /// `emitted_len`, because those rows have already been inserted into terminal history.
    fn rebuild_queue_from_render(&mut self) {
        self.state.clear_queue();
        let target_len = self.rendered_lines.len().max(self.emitted_len);
        if self.emitted_len < target_len {
            self.state
                .enqueue(self.rendered_lines[self.emitted_len..target_len].to_vec());
        }
        self.enqueued_len = target_len;
    }

    fn process_live_table_preview_source(&mut self, source: &str) -> bool {
        let mut handled = false;
        let mut pending_markdown = String::new();
        let mut preview_lines = Vec::new();

        for line in source.split_inclusive('\n') {
            let line_without_newline = line.trim_end_matches(['\r', '\n']);
            if let Some(table) = &mut self.live_table {
                if is_live_pipe_table_row(line_without_newline) {
                    if live_pipe_table_alignments(line_without_newline).is_some() {
                        handled = true;
                        continue;
                    }

                    let cells = parse_live_pipe_table_cells(line_without_newline);
                    if table.has_rows {
                        preview_lines.push(live_table_border(&table.widths, '├', '┼', '┤'));
                    }
                    preview_lines.extend(live_table_row(
                        &cells,
                        &table.widths,
                        &table.alignments,
                        &self.cwd,
                        /*is_header*/ false,
                    ));
                    table.has_rows = true;
                    handled = true;
                    continue;
                }

                let Some(table) = self.live_table.take() else {
                    continue;
                };
                preview_lines.push(live_table_border(&table.widths, '└', '┴', '┘'));
                handled = true;
                pending_markdown.push_str(line);
                continue;
            }

            if let Some(candidate_header) = self.live_table_candidate_header.take() {
                let header_cells =
                    parse_live_pipe_table_cells(candidate_header.trim_end_matches(['\r', '\n']));
                if let Some(alignments) = live_pipe_table_alignments(line_without_newline)
                    && alignments.len() == header_cells.len()
                {
                    if !pending_markdown.is_empty() {
                        append_markdown(
                            &pending_markdown,
                            self.width,
                            Some(self.cwd.as_path()),
                            &mut preview_lines,
                        );
                        pending_markdown.clear();
                    }

                    self.live_table = Some(LivePipeTable {
                        widths: live_table_widths(self.width, &header_cells, &self.cwd),
                        alignments,
                        has_rows: true,
                    });
                    if let Some(table) = &self.live_table {
                        preview_lines.push(live_table_border(&table.widths, '┌', '┬', '┐'));
                        preview_lines.extend(live_table_row(
                            &header_cells,
                            &table.widths,
                            &table.alignments,
                            &self.cwd,
                            /*is_header*/ true,
                        ));
                    }
                    handled = true;
                    continue;
                }

                pending_markdown.push_str(&candidate_header);
            }

            if is_live_pipe_table_row(line_without_newline) {
                if live_pipe_table_alignments(line_without_newline).is_some() {
                    pending_markdown.push_str(line);
                } else {
                    self.live_table_candidate_header = Some(line.to_string());
                    handled = true;
                }
            } else {
                pending_markdown.push_str(line);
            }
        }

        if !pending_markdown.is_empty() {
            append_markdown(
                &pending_markdown,
                self.width,
                Some(self.cwd.as_path()),
                &mut preview_lines,
            );
        }

        if handled {
            self.rendered_lines.extend(preview_lines.clone());
            self.state.enqueue(preview_lines);
            self.enqueued_len = self.rendered_lines.len();
        }
        handled
    }
}

fn is_live_pipe_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 3
}

fn live_pipe_table_alignments(line: &str) -> Option<Vec<Alignment>> {
    parse_live_pipe_table_cells(line)
        .iter()
        .map(|cell| live_pipe_table_alignment(cell))
        .collect()
}

fn live_pipe_table_alignment(cell: &str) -> Option<Alignment> {
    let cell = cell.trim();
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    let dashes = cell.trim_matches(':');
    if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
        return None;
    }

    match (left, right) {
        (true, true) => Some(Alignment::Center),
        (true, false) => Some(Alignment::Left),
        (false, true) => Some(Alignment::Right),
        (false, false) => Some(Alignment::None),
    }
}

fn parse_live_pipe_table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn live_table_widths(width: Option<usize>, cells: &[String], cwd: &Path) -> Vec<usize> {
    let column_count = cells.len().max(1);
    let natural: Vec<usize> = cells
        .iter()
        .map(|cell| live_table_cell_width(cell, cwd).max(3))
        .collect();
    let Some(width) = width else {
        return natural;
    };
    let fixed_width = column_count * 3 + 1;
    let available = width.saturating_sub(fixed_width).max(column_count);
    let base = (available / column_count).max(1);
    let mut widths = vec![base; column_count];
    for width in widths.iter_mut().take(available % column_count) {
        *width += 1;
    }
    widths
}

fn live_table_border(widths: &[usize], left: char, middle: char, right: char) -> Line<'static> {
    let mut line = left.to_string();
    for (index, width) in widths.iter().copied().enumerate() {
        line.push_str(&"─".repeat(width + 2));
        line.push(if index + 1 < widths.len() {
            middle
        } else {
            right
        });
    }
    Line::from(line)
}

fn live_table_cell_width(cell: &str, cwd: &Path) -> usize {
    render_live_table_cell(cell, cwd)
        .into_iter()
        .map(|spans| spans_display_width(&spans))
        .max()
        .unwrap_or(0)
}

fn render_live_table_cell(cell: &str, cwd: &Path) -> Vec<Vec<Span<'static>>> {
    let rendered = render_markdown_text_with_width_and_cwd(cell, /*width*/ None, Some(cwd));
    if rendered.lines.is_empty() {
        return vec![Vec::new()];
    }

    rendered.lines.into_iter().map(|line| line.spans).collect()
}

fn live_table_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    cwd: &Path,
    is_header: bool,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .copied()
        .enumerate()
        .map(|(index, width)| {
            let cell = cells.get(index).map(String::as_str).unwrap_or("");
            wrap_live_table_cell(cell, width, cwd, is_header)
        })
        .collect();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let top_padding: Vec<usize> = wrapped
        .iter()
        .map(|cell| row_height.saturating_sub(cell.len()) / 2)
        .collect();

    (0..row_height)
        .map(|line_index| {
            let mut spans = vec!["│".into()];
            for (column_index, ((cell, top_padding), width)) in wrapped
                .iter()
                .zip(top_padding.iter().copied())
                .zip(widths.iter().copied())
                .enumerate()
            {
                let line_spans = line_index
                    .checked_sub(top_padding)
                    .and_then(|index| cell.get(index))
                    .cloned()
                    .unwrap_or_else(Vec::new);
                let alignment = alignments
                    .get(column_index)
                    .copied()
                    .unwrap_or(Alignment::None);
                spans.extend(pad_live_table_cell_line(line_spans, width, alignment));
                spans.push("│".into());
            }
            Line::from(spans)
        })
        .collect()
}

fn wrap_live_table_cell(
    cell: &str,
    width: usize,
    cwd: &Path,
    is_header: bool,
) -> Vec<Vec<Span<'static>>> {
    let mut lines = render_live_table_cell(cell, cwd);
    if is_header {
        for line in &mut lines {
            for span in line {
                span.style = span.style.patch(ratatui::style::Style::new().bold());
            }
        }
    }

    let mut wrapped = Vec::new();
    for spans in lines {
        let line = Line::from(spans);
        let wrapped_lines = adaptive_wrap_line(&line, RtOptions::new(width.max(1)));
        if wrapped_lines.is_empty() {
            wrapped.push(Vec::new());
        } else {
            wrapped.extend(wrapped_lines.iter().map(|line| line_to_static(line).spans));
        }
    }

    if wrapped.is_empty() {
        vec![Vec::new()]
    } else {
        wrapped
    }
}

fn pad_live_table_cell_line(
    spans: Vec<Span<'static>>,
    width: usize,
    alignment: Alignment,
) -> Vec<Span<'static>> {
    let content_width = spans_display_width(&spans);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        Alignment::None | Alignment::Left => (0, padding),
        Alignment::Center => {
            let left = padding / 2;
            (left, padding - left)
        }
        Alignment::Right => (padding, 0),
    };

    let mut padded = vec![Span::from(" ")];
    if left > 0 {
        padded.push(Span::from(" ".repeat(left)));
    }
    padded.extend(spans);
    if right > 0 {
        padded.push(Span::from(" ".repeat(right)));
    }
    padded.push(Span::from(" "));
    padded
}

fn spans_display_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Controls newline-gated streaming for assistant messages.
///
/// The controller emits transient `AgentMessageCell`s for live display and returns raw markdown
/// source on `finalize` so the app can replace those transient cells with a source-backed
/// `AgentMarkdownCell`. Callers should use `set_width` on terminal resize; rebuilding the queue
/// from already emitted cells would duplicate output instead of preserving the stream position.
pub(crate) struct StreamController {
    core: StreamCore,
    header_emitted: bool,
}

impl StreamController {
    /// Create a stream controller that renders markdown relative to the given width and cwd.
    ///
    /// `width` is the content width available to markdown rendering, not necessarily the full
    /// terminal width. Passing a stale width after resize will keep queued live output wrapped for
    /// the old viewport until app-level reflow repairs the finalized transcript.
    pub(crate) fn new(width: Option<usize>, cwd: &Path) -> Self {
        Self {
            core: StreamCore::new(width, cwd),
            header_emitted: false,
        }
    }

    /// Push a raw model delta and return whether it produced queued complete lines.
    ///
    /// Deltas are committed only through newline boundaries. A `false` return can still mean source
    /// was buffered; it only means no newly renderable complete line is ready for live emission.
    pub(crate) fn push(&mut self, delta: &str) -> bool {
        self.core.push_delta(delta)
    }

    /// Finish the stream and return the final transient cell plus accumulated markdown source.
    ///
    /// The source is `None` only when the stream never accumulated content. Callers that discard the
    /// returned source cannot later consolidate the transcript into a width-sensitive finalized
    /// cell.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let remaining = self.core.finalize_remaining();
        if self.core.raw_source.is_empty() {
            self.core.reset();
            return (None, None);
        }

        let source = std::mem::take(&mut self.core.raw_source);
        let out = self.emit(remaining);
        self.core.reset();
        (out, Some(source))
    }

    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick();
        (self.emit(step), self.core.is_idle())
    }

    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick_batch(max_lines);
        (self.emit(step), self.core.is_idle())
    }

    pub(crate) fn queued_lines(&self) -> usize {
        self.core.queued_lines()
    }

    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.core.oldest_queued_age(now)
    }

    pub(crate) fn clear_queue(&mut self) {
        self.core.clear_queue();
    }

    pub(crate) fn set_width(&mut self, width: Option<usize>) {
        self.core.set_width(width);
    }

    fn emit(&mut self, lines: Vec<Line<'static>>) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() {
            return None;
        }
        Some(Box::new(history_cell::AgentMessageCell::new(lines, {
            let header_emitted = self.header_emitted;
            self.header_emitted = true;
            !header_emitted
        })))
    }
}

/// Controls newline-gated streaming for proposed plan markdown.
///
/// This follows the same source-retention contract as `StreamController`, but wraps emitted lines
/// in the proposed-plan header, padding, and style. Finalization must return source for
/// `ProposedPlanCell`; otherwise a resized finalized plan would keep the transient stream shape.
pub(crate) struct PlanStreamController {
    core: StreamCore,
    header_emitted: bool,
    top_padding_emitted: bool,
}

impl PlanStreamController {
    /// Create a proposed-plan stream controller that renders markdown relative to the given cwd.
    ///
    /// The width has the same meaning as in `StreamController`: it is the markdown body width, and
    /// callers must update it when the terminal width changes.
    pub(crate) fn new(width: Option<usize>, cwd: &Path) -> Self {
        Self {
            core: StreamCore::new(width, cwd),
            header_emitted: false,
            top_padding_emitted: false,
        }
    }

    /// Push a raw proposed-plan delta and return whether it produced queued complete lines.
    ///
    /// Source may be buffered even when this returns `false`; callers should continue ticking only
    /// when queued lines exist.
    pub(crate) fn push(&mut self, delta: &str) -> bool {
        self.core.push_delta(delta)
    }

    /// Finish the plan stream and return the final transient cell plus accumulated markdown source.
    ///
    /// The returned source is consumed by app-level consolidation to create the source-backed
    /// `ProposedPlanCell` used for later resize reflow.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let remaining = self.core.finalize_remaining();
        if self.core.raw_source.is_empty() {
            self.core.reset();
            return (None, None);
        }

        let source = std::mem::take(&mut self.core.raw_source);
        let out = self.emit(remaining, /*include_bottom_padding*/ true);
        self.core.reset();
        (out, Some(source))
    }

    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick();
        (
            self.emit(step, /*include_bottom_padding*/ false),
            self.core.is_idle(),
        )
    }

    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick_batch(max_lines);
        (
            self.emit(step, /*include_bottom_padding*/ false),
            self.core.is_idle(),
        )
    }

    pub(crate) fn queued_lines(&self) -> usize {
        self.core.queued_lines()
    }

    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.core.oldest_queued_age(now)
    }

    pub(crate) fn clear_queue(&mut self) {
        self.core.clear_queue();
    }

    pub(crate) fn set_width(&mut self, width: Option<usize>) {
        self.core.set_width(width);
    }

    fn emit(
        &mut self,
        lines: Vec<Line<'static>>,
        include_bottom_padding: bool,
    ) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() && !include_bottom_padding {
            return None;
        }

        let mut out_lines: Vec<Line<'static>> = Vec::with_capacity(4);
        let is_stream_continuation = self.header_emitted;
        if !self.header_emitted {
            out_lines.push(vec!["• ".dim(), "Proposed Plan".bold()].into());
            out_lines.push(Line::from(" "));
            self.header_emitted = true;
        }

        let mut plan_lines: Vec<Line<'static>> = Vec::with_capacity(4);
        if !self.top_padding_emitted {
            plan_lines.push(Line::from(" "));
            self.top_padding_emitted = true;
        }
        plan_lines.extend(lines);
        if include_bottom_padding {
            plan_lines.push(Line::from(" "));
        }

        let plan_style = proposed_plan_style();
        let plan_lines = prefix_lines(plan_lines, "  ".into(), "  ".into())
            .into_iter()
            .map(|line| line.style(plan_style))
            .collect::<Vec<_>>();
        out_lines.extend(plan_lines);

        Some(Box::new(history_cell::new_proposed_plan_stream(
            out_lines,
            is_stream_continuation,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Modifier;

    fn test_cwd() -> PathBuf {
        std::env::temp_dir()
    }

    fn stream_controller(width: Option<usize>) -> StreamController {
        StreamController::new(width, &test_cwd())
    }

    fn plan_stream_controller(width: Option<usize>) -> PlanStreamController {
        PlanStreamController::new(width, &test_cwd())
    }

    fn lines_to_plain_strings(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.clone())
                    .collect::<String>()
            })
            .collect()
    }

    fn collect_streamed_lines(deltas: &[&str], width: Option<usize>) -> Vec<String> {
        let mut ctrl = stream_controller(width);
        let mut lines = Vec::new();
        for delta in deltas {
            ctrl.push(delta);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        if let (Some(cell), _source) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }
        lines_to_plain_strings(&lines)
            .into_iter()
            .map(|line| line.chars().skip(2).collect::<String>())
            .collect()
    }

    fn collect_plan_streamed_lines(deltas: &[&str], width: Option<usize>) -> Vec<String> {
        let mut ctrl = plan_stream_controller(width);
        let mut lines = Vec::new();
        for delta in deltas {
            ctrl.push(delta);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        if let (Some(cell), _source) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }
        lines_to_plain_strings(&lines)
    }

    #[test]
    fn controller_set_width_rebuilds_queued_lines() {
        let mut ctrl = stream_controller(Some(120));
        let delta = "This is a long line that should wrap into multiple rows when resized.\n";
        assert!(ctrl.push(delta));
        assert_eq!(ctrl.queued_lines(), 1);

        ctrl.set_width(Some(24));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        let rendered = lines_to_plain_strings(
            &cell
                .expect("expected resized queued lines")
                .transcript_lines(u16::MAX),
        );

        assert!(idle);
        assert!(
            rendered.len() > 1,
            "expected resized content to occupy multiple lines, got {rendered:?}",
        );
    }

    #[test]
    fn controller_set_width_no_duplicate_after_emit() {
        let mut ctrl = stream_controller(Some(120));
        let line =
            "This is a long line that definitely wraps when the terminal shrinks to 24 columns.\n";
        ctrl.push(line);
        let (cell, _) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(cell.is_some(), "expected emitted cell");
        assert_eq!(ctrl.queued_lines(), 0);

        ctrl.set_width(Some(24));

        assert_eq!(
            ctrl.queued_lines(),
            0,
            "already-emitted content must not be re-queued after resize",
        );
    }

    #[test]
    fn controller_tick_batch_zero_is_noop() {
        let mut ctrl = stream_controller(Some(80));
        assert!(ctrl.push("line one\n"));
        assert_eq!(ctrl.queued_lines(), 1);

        let (cell, idle) = ctrl.on_commit_tick_batch(/*max_lines*/ 0);
        assert!(cell.is_none(), "batch size 0 should not emit lines");
        assert!(!idle, "batch size 0 should not drain queued lines");
        assert_eq!(
            ctrl.queued_lines(),
            1,
            "queue depth should remain unchanged"
        );
    }

    #[test]
    fn controller_finalize_returns_raw_source_for_consolidation() {
        let mut ctrl = stream_controller(Some(80));
        assert!(ctrl.push("hello\n"));
        let (_cell, source) = ctrl.finalize();
        assert_eq!(source, Some("hello\n".to_string()));
    }

    #[test]
    fn plan_controller_finalize_returns_raw_source_for_consolidation() {
        let mut ctrl = plan_stream_controller(Some(80));
        assert!(ctrl.push("- step\n"));
        let (_cell, source) = ctrl.finalize();
        assert_eq!(source, Some("- step\n".to_string()));
    }

    #[test]
    fn simple_lines_stream_in_order() {
        let actual = collect_streamed_lines(&["hello\n", "world\n"], Some(80));
        assert_eq!(actual, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn pipe_table_rows_stream_as_live_preview() {
        let mut ctrl = stream_controller(Some(20));

        assert!(!ctrl.push("| A | B |\n"));
        assert_eq!(ctrl.queued_lines(), 0);

        assert!(ctrl.push("| --- | --- |\n"));
        assert_eq!(ctrl.queued_lines(), 2);
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected header preview")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "┌─────────┬────────┐".to_string(),
                "│ A       │ B      │".to_string(),
            ]
        );

        assert!(ctrl.push("| C | D |\n"));
        assert_eq!(ctrl.queued_lines(), 2);
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected body row preview")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "├─────────┼────────┤".to_string(),
                "│ C       │ D      │".to_string(),
            ]
        );

        let (cell, source) = ctrl.finalize();
        assert_eq!(
            source,
            Some("| A | B |\n| --- | --- |\n| C | D |\n".to_string())
        );
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected table closing border")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(lines, vec!["└─────────┴────────┘".to_string()]);
    }

    #[test]
    fn pipe_table_live_preview_renders_inline_markdown() {
        let mut ctrl = stream_controller(Some(60));

        assert!(!ctrl.push("| Type | Effect |\n"));
        assert!(ctrl.push("| --- | --- |\n"));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        assert!(cell.is_some(), "expected header preview");

        assert!(ctrl.push("| bold | **bold** |\n"));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = cell
            .expect("expected body row preview")
            .transcript_lines(u16::MAX);
        let plain = lines_to_plain_strings(&lines);
        assert!(
            plain.iter().all(|line| !line.contains("**")),
            "live preview should not leak raw emphasis markers: {plain:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "expected bold cell content to keep bold styling: {lines:?}"
        );
    }

    #[test]
    fn short_alignment_delimiters_stream_as_live_table() {
        let mut ctrl = stream_controller(Some(30));

        assert!(!ctrl.push("| L | C | R |\n"));
        assert_eq!(ctrl.queued_lines(), 0);

        assert!(ctrl.push("| :-- | :-: | --: |\n"));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected header preview")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "┌─────────┬─────────┬────────┐".to_string(),
                "│ L       │    C    │      R │".to_string(),
            ]
        );

        assert!(ctrl.push("| a | b | 123 |\n"));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected body row preview")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "├─────────┼─────────┼────────┤".to_string(),
                "│ a       │    b    │    123 │".to_string(),
            ]
        );
    }

    #[test]
    fn pipe_rows_without_delimiter_do_not_stream_as_live_table() {
        let mut ctrl = stream_controller(Some(80));

        assert!(!ctrl.push("| A | B |\n"));
        assert_eq!(ctrl.queued_lines(), 0);

        assert!(ctrl.push("| C | D |\n"));
        assert_eq!(ctrl.queued_lines(), 1);
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle);
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected first non-table pipe row as text")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(lines, vec!["| A | B |".to_string()]);

        let (cell, source) = ctrl.finalize();
        assert_eq!(source, Some("| A | B |\n| C | D |\n".to_string()));
        let lines = lines_to_plain_strings(
            &cell
                .expect("expected final non-table pipe row as text")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(lines, vec!["| C | D |".to_string()]);
    }

    #[test]
    fn plan_lines_stream_in_order() {
        let actual = collect_plan_streamed_lines(&["- one\n", "- two\n"], Some(80));
        assert!(
            actual.iter().any(|line| line.contains("Proposed Plan")),
            "expected plan header in streamed plan: {actual:?}",
        );
        assert!(
            actual.iter().any(|line| line.contains("one")),
            "expected plan body in streamed plan: {actual:?}",
        );
    }
}
