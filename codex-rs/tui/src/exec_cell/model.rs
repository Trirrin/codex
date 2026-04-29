//! Data model for grouped exec-call history cells in the TUI transcript.
//!
//! An `ExecCell` can represent either a single command or an "exploring" group of related read/
//! list/search commands. The chat widget relies on stable `call_id` matching to route progress and
//! end events into the right cell, and it treats "call id not found" as a real signal (for
//! example, an orphan end that should render as a separate history entry).

use std::time::Duration;
use std::time::Instant;

use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::ExecCommandRunMode;
use codex_protocol::protocol::ExecCommandSource;

#[derive(Clone, Debug, Default)]
pub(crate) struct CommandOutput {
    pub(crate) exit_code: i32,
    /// The aggregated stderr + stdout interleaved.
    pub(crate) aggregated_output: String,
    /// The formatted output of the command, as seen by the model.
    pub(crate) formatted_output: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecCall {
    pub(crate) call_id: String,
    pub(crate) command: Vec<String>,
    pub(crate) parsed: Vec<ParsedCommand>,
    pub(crate) output: Option<CommandOutput>,
    pub(crate) auto_review_approved: bool,
    pub(crate) source: ExecCommandSource,
    pub(crate) run_mode: Option<ExecCommandRunMode>,
    pub(crate) start_time: Option<Instant>,
    pub(crate) duration: Option<Duration>,
    pub(crate) interaction_input: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExploredToolsDisplay {
    #[default]
    Compact,
    Detailed,
}

impl ExploredToolsDisplay {
    pub(crate) fn from_compact_explored_tools(compact_explored_tools: bool) -> Self {
        if compact_explored_tools {
            Self::Compact
        } else {
            Self::Detailed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecCellDisplayOptions {
    pub(crate) animations_enabled: bool,
    pub(crate) explored_tools_display: ExploredToolsDisplay,
}

#[derive(Debug)]
pub(crate) struct ExecCell {
    pub(crate) calls: Vec<ExecCall>,
    animations_enabled: bool,
    explored_tools_display: ExploredToolsDisplay,
}

impl ExecCell {
    pub(crate) fn new(call: ExecCall, animations_enabled: bool) -> Self {
        Self {
            calls: vec![call],
            animations_enabled,
            explored_tools_display: ExploredToolsDisplay::default(),
        }
    }

    pub(crate) fn with_added_call(
        &self,
        call_id: String,
        command: Vec<String>,
        parsed: Vec<ParsedCommand>,
        source: ExecCommandSource,
        run_mode: Option<ExecCommandRunMode>,
        interaction_input: Option<String>,
    ) -> Option<Self> {
        let call = ExecCall {
            call_id,
            command,
            parsed,
            output: None,
            auto_review_approved: false,
            source,
            run_mode,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input,
        };
        if self.is_exploring_cell() && Self::is_exploring_call(&call) {
            Some(Self {
                calls: [self.calls.clone(), vec![call]].concat(),
                animations_enabled: self.animations_enabled,
                explored_tools_display: self.explored_tools_display,
            })
        } else {
            None
        }
    }

    /// Marks the most recently matching call as finished and returns whether a call was found.
    ///
    /// Callers should treat `false` as a routing mismatch rather than silently ignoring it. The
    /// chat widget uses that signal to avoid attaching an orphan `exec_end` event to an unrelated
    /// active exploring cell, which would incorrectly collapse two transcript entries together.
    pub(crate) fn complete_call(
        &mut self,
        call_id: &str,
        output: CommandOutput,
        duration: Duration,
        run_mode: Option<ExecCommandRunMode>,
    ) -> bool {
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) else {
            return false;
        };
        if run_mode.is_some() {
            call.run_mode = run_mode;
        }
        call.output = Some(output);
        call.duration = Some(duration);
        call.start_time = None;
        true
    }

    pub(crate) fn should_flush(&self) -> bool {
        !self.is_exploring_cell() && self.calls.iter().all(|c| c.start_time.is_none())
    }

    pub(crate) fn mark_failed(&mut self) {
        for call in self.calls.iter_mut() {
            if call.output.is_none() {
                let elapsed = call
                    .start_time
                    .map(|st| st.elapsed())
                    .unwrap_or_else(|| Duration::from_millis(0));
                call.start_time = None;
                call.duration = Some(elapsed);
                call.output = Some(CommandOutput {
                    exit_code: 1,
                    formatted_output: String::new(),
                    aggregated_output: String::new(),
                });
            }
        }
    }

    pub(crate) fn is_exploring_cell(&self) -> bool {
        self.calls.iter().all(Self::is_exploring_call)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.calls.iter().any(|c| c.start_time.is_some())
    }

    pub(crate) fn active_start_time(&self) -> Option<Instant> {
        self.calls.iter().find_map(|c| c.start_time)
    }

    pub(crate) fn animations_enabled(&self) -> bool {
        self.animations_enabled
    }

    pub(crate) fn explored_tools_display(&self) -> ExploredToolsDisplay {
        self.explored_tools_display
    }

    pub(crate) fn with_explored_tools_display(mut self, display: ExploredToolsDisplay) -> Self {
        self.explored_tools_display = display;
        self
    }

    pub(crate) fn iter_calls(&self) -> impl Iterator<Item = &ExecCall> {
        self.calls.iter()
    }

    pub(crate) fn append_output(&mut self, call_id: &str, chunk: &str) -> bool {
        if chunk.is_empty() {
            return false;
        }
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) else {
            return false;
        };
        if call.is_background_unified_exec_startup() {
            return false;
        }
        let output = call.output.get_or_insert_with(CommandOutput::default);
        output.aggregated_output.push_str(chunk);
        true
    }

    pub(crate) fn mark_auto_review_approved(&mut self, call_id: &str) -> bool {
        let Some(call) = self.calls.iter_mut().rev().find(|c| c.call_id == call_id) else {
            return false;
        };
        call.auto_review_approved = true;
        true
    }

    pub(super) fn is_exploring_call(call: &ExecCall) -> bool {
        !matches!(
            call.source,
            ExecCommandSource::UserShell | ExecCommandSource::UnifiedExecStartup
        ) && !call.parsed.is_empty()
            && call.parsed.iter().all(|p| {
                matches!(
                    p,
                    ParsedCommand::Read { .. }
                        | ParsedCommand::ListFiles { .. }
                        | ParsedCommand::Search { .. }
                )
            })
    }
}

impl ExecCall {
    pub(crate) fn is_user_shell_command(&self) -> bool {
        matches!(self.source, ExecCommandSource::UserShell)
    }

    pub(crate) fn is_unified_exec_interaction(&self) -> bool {
        matches!(self.source, ExecCommandSource::UnifiedExecInteraction)
    }

    pub(crate) fn is_unified_exec_startup(&self) -> bool {
        matches!(self.source, ExecCommandSource::UnifiedExecStartup)
    }

    pub(crate) fn is_background_unified_exec_startup(&self) -> bool {
        self.is_unified_exec_startup()
            && matches!(self.run_mode, Some(ExecCommandRunMode::Background))
    }
}
