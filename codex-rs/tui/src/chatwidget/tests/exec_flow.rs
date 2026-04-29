use super::*;
use pretty_assertions::assert_eq;
use std::time::Duration;
use std::time::Instant;

#[tokio::test]
async fn exec_approval_emits_proposed_command_and_decision_history() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Trigger an exec approval request with a short, single-line command
    let ev = ExecApprovalRequestEvent {
        call_id: "call-short".into(),
        approval_id: Some("call-short".into()),
        turn_id: "turn-short".into(),
        command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some(
            "this is a test reason such as one that would be produced by the model".into(),
        ),
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-short".into(),
        msg: EventMsg::ExecApprovalRequest(ev),
    });

    let proposed_cells = drain_insert_history(&mut rx);
    assert!(
        proposed_cells.is_empty(),
        "expected approval request to render via modal without emitting history cells"
    );

    // The approval modal should display the command snippet for user confirmation.
    let area = Rect::new(0, 0, 80, chat.desired_height(/*width*/ 80));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buf);
    assert_chatwidget_snapshot!("exec_approval_modal_exec", format!("{buf:?}"));

    // Approve via keyboard and verify a concise decision history line is added
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let decision = drain_insert_history(&mut rx)
        .pop()
        .expect("expected decision cell in history");
    assert_chatwidget_snapshot!(
        "exec_approval_history_decision_approved_short",
        lines_to_single_string(&decision)
    );
}

#[test]
fn app_server_exec_approval_request_splits_shell_wrapped_command() {
    let script = r#"python3 -c 'print("Hello, world!")'"#;
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            approval_id: Some("approval-1".to_string()),
            reason: None,
            network_approval_context: None,
            command: Some(
                shlex::try_join(["/bin/zsh", "-lc", script])
                    .expect("round-trippable shell wrapper"),
            ),
            cwd: Some(test_path_buf("/tmp").abs()),
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
        &test_path_buf("/tmp").abs(),
    );

    assert_eq!(
        request.command,
        vec![
            "/bin/zsh".to_string(),
            "-lc".to_string(),
            script.to_string(),
        ]
    );
}

#[tokio::test]
async fn exec_approval_uses_approval_id_when_present() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.handle_codex_event(Event {
        id: "sub-short".into(),
        msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id: "call-parent".into(),
            approval_id: Some("approval-subcommand".into()),
            turn_id: "turn-short".into(),
            command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
            cwd: AbsolutePathBuf::current_dir().expect("current dir"),
            reason: Some(
                "this is a test reason such as one that would be produced by the model".into(),
            ),
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: None,
            parsed_cmd: vec![],
        }),
    });

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    let mut found = false;
    while let Ok(app_ev) = rx.try_recv() {
        if let AppEvent::SubmitThreadOp {
            op: Op::ExecApproval { id, decision, .. },
            ..
        } = app_ev
        {
            assert_eq!(id, "approval-subcommand");
            assert_matches!(decision, codex_protocol::protocol::ReviewDecision::Approved);
            found = true;
            break;
        }
    }
    assert!(found, "expected ExecApproval op to be sent");
}

#[tokio::test]
async fn exec_approval_decision_truncates_multiline_and_long_commands() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Multiline command: modal should show full command, history records decision only
    let ev_multi = ExecApprovalRequestEvent {
        call_id: "call-multi".into(),
        approval_id: Some("call-multi".into()),
        turn_id: "turn-multi".into(),
        command: vec!["bash".into(), "-lc".into(), "echo line1\necho line2".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some(
            "this is a test reason such as one that would be produced by the model".into(),
        ),
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-multi".into(),
        msg: EventMsg::ExecApprovalRequest(ev_multi),
    });
    let proposed_multi = drain_insert_history(&mut rx);
    assert!(
        proposed_multi.is_empty(),
        "expected multiline approval request to render via modal without emitting history cells"
    );

    let area = Rect::new(0, 0, 80, chat.desired_height(/*width*/ 80));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buf);
    let mut saw_first_line = false;
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        if row.contains("echo line1") {
            saw_first_line = true;
            break;
        }
    }
    assert!(
        saw_first_line,
        "expected modal to show first line of multiline snippet"
    );

    // Deny via keyboard; decision snippet should be single-line and elided with " ..."
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    let aborted_multi = drain_insert_history(&mut rx)
        .pop()
        .expect("expected aborted decision cell (multiline)");
    assert_chatwidget_snapshot!(
        "exec_approval_history_decision_aborted_multiline",
        lines_to_single_string(&aborted_multi)
    );

    // Very long single-line command: decision snippet should be truncated <= 80 chars with trailing ...
    let long = format!("echo {}", "a".repeat(200));
    let ev_long = ExecApprovalRequestEvent {
        call_id: "call-long".into(),
        approval_id: Some("call-long".into()),
        turn_id: "turn-long".into(),
        command: vec!["bash".into(), "-lc".into(), long],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: None,
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-long".into(),
        msg: EventMsg::ExecApprovalRequest(ev_long),
    });
    let proposed_long = drain_insert_history(&mut rx);
    assert!(
        proposed_long.is_empty(),
        "expected long approval request to avoid emitting history cells before decision"
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    let aborted_long = drain_insert_history(&mut rx)
        .pop()
        .expect("expected aborted decision cell (long)");
    assert_chatwidget_snapshot!(
        "exec_approval_history_decision_aborted_long",
        lines_to_single_string(&aborted_long)
    );
}

#[tokio::test]
async fn preamble_keeps_working_status_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());

    // Regression sequence: a preamble line is committed to history before any exec/tool event.
    // After commentary completes, the status row should be restored before subsequent work.
    chat.on_task_started();
    chat.on_agent_message_delta("Preamble line\n".to_string());
    chat.on_commit_tick();
    drain_insert_history(&mut rx);
    complete_assistant_message(
        &mut chat,
        "msg-commentary-snapshot",
        "Preamble line\n",
        Some(MessagePhase::Commentary),
    );

    let height = chat.desired_height(/*width*/ 80);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height))
        .expect("create terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw preamble + status widget");
    assert_chatwidget_snapshot!(
        "preamble_keeps_working_status",
        normalized_backend_snapshot(terminal.backend())
    );
}

#[tokio::test]
async fn unified_exec_begin_restores_status_indicator_after_preamble() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.on_task_started();
    assert_eq!(chat.bottom_pane.status_indicator_visible(), true);

    // Simulate a hidden status row during an active turn.
    chat.bottom_pane.hide_status_indicator();
    assert_eq!(chat.bottom_pane.status_indicator_visible(), false);
    assert_eq!(chat.bottom_pane.is_task_running(), true);

    begin_unified_exec_startup(&mut chat, "call-1", "proc-1", "sleep 2");

    assert_eq!(chat.bottom_pane.status_indicator_visible(), true);
}

#[tokio::test]
async fn model_activity_status_lists_current_tool_actions() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    let cwd = AbsolutePathBuf::current_dir().expect("current dir");
    let parsed_cmd = vec![
        ParsedCommand::Search {
            cmd: "rg status".to_string(),
            query: Some("status".to_string()),
            path: None,
        },
        ParsedCommand::Read {
            cmd: "sed -n 1,20p tui/src/chatwidget.rs".to_string(),
            name: "chatwidget.rs".to_string(),
            path: "tui/src/chatwidget.rs".into(),
        },
        ParsedCommand::ListFiles {
            cmd: "find tui/src -type f".to_string(),
            path: Some("tui/src".to_string()),
        },
        ParsedCommand::Unknown {
            cmd: "echo ignored".to_string(),
        },
    ];
    let begin = ExecCommandBeginEvent {
        call_id: "call-actions".to_string(),
        process_id: None,
        turn_id: "turn-1".to_string(),
        command: vec![
            "bash".to_string(),
            "-lc".to_string(),
            "rg status".to_string(),
        ],
        cwd: cwd.clone(),
        parsed_cmd: parsed_cmd.clone(),
        source: ExecCommandSource::Agent,
        run_mode: None,
        interaction_input: None,
    };

    chat.on_exec_command_begin(begin);

    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Searching, Reading and Globing");
    assert_eq!(
        chat.run_state_status_text(),
        "Searching, Reading and Globing"
    );

    chat.on_exec_command_end(ExecCommandEndEvent {
        call_id: "call-actions".to_string(),
        process_id: None,
        turn_id: "turn-1".to_string(),
        command: vec![
            "bash".to_string(),
            "-lc".to_string(),
            "rg status".to_string(),
        ],
        cwd,
        parsed_cmd,
        source: ExecCommandSource::Agent,
        interaction_input: None,
        stdout: String::new(),
        stderr: String::new(),
        aggregated_output: String::new(),
        exit_code: 0,
        duration: Duration::from_millis(1),
        formatted_output: String::new(),
        status: codex_protocol::protocol::ExecCommandStatus::Completed,
    });

    assert_eq!(chat.current_status.header, "Working");
}

#[tokio::test]
async fn explored_display_uses_relative_paths_for_cwd_files() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config.compact_explored_tools = false;
    chat.on_task_started();
    let cwd = test_project_path().abs();
    chat.config.cwd = cwd.clone();
    let read_path = test_project_path().join("src/main.py");
    let search_path = test_project_path().join("src");

    chat.on_exec_command_begin(ExecCommandBeginEvent {
        call_id: "call-relative".to_string(),
        process_id: None,
        turn_id: "turn-1".to_string(),
        command: vec!["read_file".to_string(), read_path.display().to_string()],
        cwd,
        parsed_cmd: vec![
            ParsedCommand::Read {
                cmd: "read_file --start-line=85 --end-line=97".to_string(),
                name: read_path.display().to_string(),
                path: read_path,
            },
            ParsedCommand::Search {
                cmd: "grep_file".to_string(),
                query: Some("class Model".to_string()),
                path: Some(search_path.display().to_string()),
            },
        ],
        source: ExecCommandSource::Agent,
        run_mode: None,
        interaction_input: None,
    });

    let blob = active_blob(&chat);
    assert!(blob.contains("Read src/main.py (lines 85-97)"));
    assert!(blob.contains("Grep class Model in src"));
    assert!(!blob.contains(&test_path_display("/tmp/project")));
}

#[tokio::test]
async fn reasoning_status_takes_priority_over_tool_activity() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    let cwd = AbsolutePathBuf::current_dir().expect("current dir");
    chat.on_exec_command_begin(ExecCommandBeginEvent {
        call_id: "call-search".to_string(),
        process_id: None,
        turn_id: "turn-1".to_string(),
        command: vec![
            "bash".to_string(),
            "-lc".to_string(),
            "rg status".to_string(),
        ],
        cwd,
        parsed_cmd: vec![ParsedCommand::Search {
            cmd: "rg status".to_string(),
            query: Some("status".to_string()),
            path: None,
        }],
        source: ExecCommandSource::Agent,
        run_mode: None,
        interaction_input: None,
    });
    assert_eq!(chat.current_status.header, "Searching");

    chat.on_agent_reasoning_delta("checking".to_string());
    assert_eq!(chat.current_status.header, "Thinking");
    assert_eq!(chat.run_state_status_text(), "Thinking");

    chat.reasoning_buffer.clear();
    chat.on_agent_reasoning_delta("**Inspecting status**\n".to_string());
    assert_eq!(chat.current_status.header, "Inspecting status");
    assert_eq!(chat.run_state_status_text(), "Inspecting status");
}

#[tokio::test]
async fn dynamic_tool_request_updates_timed_status_header() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    chat.handle_codex_event(Event {
        id: "dynamic-write".to_string(),
        msg: EventMsg::DynamicToolCallRequest(
            codex_protocol::dynamic_tools::DynamicToolCallRequest {
                call_id: "dynamic-write".to_string(),
                turn_id: "turn-1".to_string(),
                namespace: None,
                tool: "write".to_string(),
                arguments: serde_json::json!({}),
            },
        ),
    });

    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Writing");

    chat.handle_codex_event(Event {
        id: "dynamic-write-done".to_string(),
        msg: EventMsg::DynamicToolCallResponse(
            codex_protocol::protocol::DynamicToolCallResponseEvent {
                call_id: "dynamic-write".to_string(),
                turn_id: "turn-1".to_string(),
                namespace: None,
                tool: "write".to_string(),
                arguments: serde_json::json!({}),
                content_items: Vec::new(),
                success: true,
                error: None,
                duration: Duration::from_millis(1),
            },
        ),
    });

    assert_eq!(chat.current_status.header, "Working");
}

#[tokio::test]
async fn output_delta_updates_timed_status_tokens_immediately() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    chat.on_agent_message_delta("a".to_string());

    let width: u16 = 80;
    let height = chat.desired_height(width);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("create terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw chatwidget");
    let rendered = normalized_backend_snapshot(terminal.backend());
    assert!(
        rendered.contains("1 token"),
        "expected first output char to refresh token estimate, got {rendered:?}"
    );
}

#[tokio::test]
async fn tool_call_content_contributes_to_timed_status_tokens() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.estimated_output_tokens(), Some(0));

    chat.handle_codex_event(Event {
        id: "dynamic-write".to_string(),
        msg: EventMsg::DynamicToolCallRequest(
            codex_protocol::dynamic_tools::DynamicToolCallRequest {
                call_id: "dynamic-write".to_string(),
                turn_id: "turn-1".to_string(),
                namespace: None,
                tool: "write".to_string(),
                arguments: serde_json::json!({
                    "cmd": "write a long enough payload to count as tool output"
                }),
            },
        ),
    });

    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert!(
        status
            .estimated_output_tokens()
            .is_some_and(|tokens| tokens > 0),
        "expected dynamic tool request content to count toward output tokens"
    );
}

#[tokio::test]
async fn unified_exec_begin_restores_working_status_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.on_task_started();
    chat.on_agent_message_delta("Preamble line\n".to_string());
    chat.on_commit_tick();
    drain_insert_history(&mut rx);

    begin_unified_exec_startup(&mut chat, "call-1", "proc-1", "sleep 2");

    let width: u16 = 80;
    let height = chat.desired_height(width);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("create terminal");
    terminal.set_viewport_area(Rect::new(0, 0, width, height));
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw chatwidget");
    assert_chatwidget_snapshot!(
        "unified_exec_begin_restores_working_status",
        normalized_backend_snapshot(terminal.backend())
    );
}

#[tokio::test]
async fn exec_history_cell_shows_working_then_completed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Begin command
    let begin = begin_exec(&mut chat, "call-1", "echo done");

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 0, "no exec cell should have been flushed yet");

    // End command successfully
    end_exec(&mut chat, begin, "done", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        0,
        "completed exec cell should update in place, not flush immediately"
    );
    let blob = active_blob(&chat);
    // New behavior: no glyph markers; ensure command is shown and no panic.
    assert!(
        blob.contains("• Ran"),
        "expected summary header present: {blob:?}"
    );
    assert!(
        blob.contains("echo done"),
        "expected command text to be present: {blob:?}"
    );
}

#[tokio::test]
async fn exec_history_cell_shows_working_then_failed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Begin command
    let begin = begin_exec(&mut chat, "call-2", "false");
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 0, "no exec cell should have been flushed yet");

    // End command with failure
    end_exec(&mut chat, begin, "", "Bloop", /*exit_code*/ 2);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        0,
        "failed exec cell should update in place, not flush immediately"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("• Ran false"),
        "expected command and header text present: {blob:?}"
    );
    assert!(blob.to_lowercase().contains("bloop"), "expected error text");
}

#[tokio::test]
async fn exec_end_without_begin_uses_event_command() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let command = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "echo orphaned".to_string(),
    ];
    let parsed_cmd = codex_shell_command::parse_command::parse_command(&command);
    let cwd = AbsolutePathBuf::current_dir().expect("current dir");
    chat.handle_codex_event(Event {
        id: "call-orphan".to_string(),
        msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id: "call-orphan".to_string(),
            process_id: None,
            turn_id: "turn-1".to_string(),
            command,
            cwd,
            parsed_cmd,
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: "done".to_string(),
            stderr: String::new(),
            aggregated_output: "done".to_string(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(5),
            formatted_output: "done".to_string(),
            status: CoreExecCommandStatus::Completed,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a standalone orphan entry");
    let blob = lines_to_single_string(&cells[0]);
    assert!(
        blob.contains("• Ran echo orphaned"),
        "expected command text to come from event: {blob:?}"
    );
    assert!(
        !blob.contains("call-orphan"),
        "call id should not be rendered when event has the command: {blob:?}"
    );
}

#[tokio::test]
async fn exec_end_without_begin_does_not_flush_unrelated_running_exploring_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    begin_exec(&mut chat, "call-exploring", "cat /dev/null");
    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(active_blob(&chat).contains("Read 1 file"));

    let orphan =
        begin_unified_exec_startup(&mut chat, "call-orphan", "proc-1", "echo repro-marker");
    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        1,
        "background startup should render separately without flushing the active exploring cell"
    );
    let background_blob = lines_to_single_string(&cells[0]);
    assert!(
        background_blob.contains("• Running echo repro-marker in background"),
        "expected standalone background entry: {background_blob:?}"
    );
    assert!(
        active_blob(&chat).contains("Read 1 file"),
        "active exploring command should remain visible"
    );

    end_exec(
        &mut chat,
        orphan,
        "repro-marker\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "background end should not render a Ran entry"
    );
    let active = active_blob(&chat);
    assert!(
        active.contains("• Read 1 file"),
        "expected unrelated exploring call to remain active: {active:?}"
    );
    assert!(
        active.contains("Read 1 file"),
        "expected active exploring command to remain visible: {active:?}"
    );
    assert!(
        !active.contains("echo repro-marker"),
        "orphaned end should not replace the active exploring cell: {active:?}"
    );
}

#[tokio::test]
async fn exec_end_without_begin_flushes_completed_unrelated_exploring_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    end_exec(&mut chat, begin_ls, "", "", /*exit_code*/ 0);
    assert!(drain_insert_history(&mut rx).is_empty());
    assert!(active_blob(&chat).contains("List 1 path"));

    let orphan = begin_unified_exec_startup(&mut chat, "call-after", "proc-1", "echo after");
    end_exec(&mut chat, orphan, "after\n", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        1,
        "completed exploring cell should flush before the background entry becomes active"
    );
    let first = lines_to_single_string(&cells[0]);
    assert!(
        first.contains("• List 1 path"),
        "expected flushed exploring cell: {first:?}"
    );
    assert!(
        first.contains("List 1 path"),
        "expected flushed exploring cell: {first:?}"
    );
    let active = active_blob(&chat);
    assert!(
        active.contains("• Running echo after in background"),
        "expected background entry to remain active: {active:?}"
    );
    assert!(
        chat.active_cell.is_some(),
        "background entry should remain active while the shell keeps running"
    );
}

#[tokio::test]
async fn overlapping_exploring_exec_end_is_not_misclassified_as_orphan() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    let begin_cat = begin_exec(&mut chat, "call-cat", "cat foo.txt");
    assert!(drain_insert_history(&mut rx).is_empty());

    end_exec(&mut chat, begin_ls, "foo.txt\n", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "tracked end inside an exploring cell should not render as an orphan"
    );
    let active = active_blob(&chat);
    assert!(
        active.contains("List 1 path"),
        "expected first command still grouped: {active:?}"
    );
    assert!(
        active.contains("Read 1 file"),
        "expected second running command to stay in the same active cell: {active:?}"
    );
    assert!(
        active.contains("• List 1 path, Read 1 file"),
        "expected grouped exploring header to remain active: {active:?}"
    );

    end_exec(&mut chat, begin_cat, "hello\n", "", /*exit_code*/ 0);
}

#[tokio::test]
async fn exec_history_shows_unified_exec_startup_commands() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "echo unified exec startup",
        ExecCommandSource::UnifiedExecStartup,
    );
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "exec begin should not flush until completion"
    );

    end_exec(
        &mut chat,
        begin,
        "echo unified exec startup\n",
        "",
        /*exit_code*/ 0,
    );

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "execute cell should update in place, not flush immediately"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("• Ran echo unified exec st…"),
        "expected startup command to render: {blob:?}"
    );
}

#[tokio::test]
async fn blocking_unified_exec_with_process_id_shows_output() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let command = vec![
        "bash".to_string(),
        "-lc".to_string(),
        "pwd && date && printf done".to_string(),
    ];
    let begin = ExecCommandBeginEvent {
        call_id: "call-blocking-process".to_string(),
        process_id: Some("12345".to_string()),
        turn_id: "turn-1".to_string(),
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        parsed_cmd: Vec::new(),
        command,
        source: ExecCommandSource::UnifiedExecStartup,
        run_mode: Some(ExecCommandRunMode::Blocking),
        interaction_input: None,
    };
    chat.handle_codex_event(Event {
        id: "call-blocking-process".to_string(),
        msg: EventMsg::ExecCommandBegin(begin.clone()),
    });

    end_exec(
        &mut chat,
        begin,
        "/tmp\nWed Apr 29 00:00:00 CST 2026\ndone\n",
        "",
        /*exit_code*/ 0,
    );

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "blocking execute cell should update in place"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("• Ran pwd && date && print…"),
        "expected blocking execute to complete instead of staying Running: {blob:?}"
    );
    assert!(
        blob.contains("/tmp") && blob.contains("done"),
        "expected blocking execute output to render: {blob:?}"
    );
}

#[tokio::test]
async fn overlapping_blocking_unified_exec_keeps_first_cell_live() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let first = begin_exec_with_source_and_run_mode(
        &mut chat,
        "call-first",
        "printf 'first start\\n'; sleep 1; printf 'first done\\n'",
        ExecCommandSource::UnifiedExecStartup,
        Some(ExecCommandRunMode::Blocking),
    );
    chat.handle_codex_event(Event {
        id: "first-output".into(),
        msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id: "call-first".into(),
            stream: ExecOutputStream::Stdout,
            chunk: b"first start\n".to_vec(),
        }),
    });

    let second = begin_exec_with_source_and_run_mode(
        &mut chat,
        "call-second",
        "printf 'second start\\n'; printf 'second done\\n'",
        ExecCommandSource::UnifiedExecStartup,
        Some(ExecCommandRunMode::Blocking),
    );

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "overlapping begin must not flush the first running cell into immutable history: {:?}",
        cells
            .iter()
            .map(|lines| lines_to_single_string(lines))
            .collect::<Vec<_>>()
    );
    let first_live = active_blob(&chat);
    assert!(
        first_live.contains("• Running printf 'first start\\…")
            && first_live.contains("first start"),
        "expected first command to stay live and keep output: {first_live:?}"
    );

    end_exec(
        &mut chat,
        second,
        "second start\nsecond done\n",
        "",
        /*exit_code*/ 0,
    );
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "second completion should render separately");
    let second_blob = lines_to_single_string(&cells[0]);
    assert!(
        second_blob.contains("• Ran printf 'second start…") && second_blob.contains("second done"),
        "expected second command to render completed: {second_blob:?}"
    );
    let first_still_live = active_blob(&chat);
    assert!(
        first_still_live.contains("• Running printf 'first start\\…")
            && first_still_live.contains("first start"),
        "first command should still be live after second completes: {first_still_live:?}"
    );

    end_exec(
        &mut chat,
        first,
        "first start\nfirst done\n",
        "",
        /*exit_code*/ 0,
    );
    let first_done = active_blob(&chat);
    assert!(
        first_done.contains("• Ran printf 'first start\\…") && first_done.contains("first done"),
        "expected first command to complete in place: {first_done:?}"
    );
}

#[tokio::test]
async fn exec_history_shows_unified_exec_tool_calls() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "ls",
        ExecCommandSource::UnifiedExecStartup,
    );
    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "execute cell should update in place, not flush immediately"
    );
    let blob = active_blob(&chat);
    assert_eq!(blob, "• Ran ls\n  └ (no output)\n");
}

#[tokio::test]
async fn exec_history_shows_shell_output_tool_calls() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-read-shell",
        "read_shell_output 10555",
        ExecCommandSource::UnifiedExecInteraction,
    );
    end_exec(
        &mut chat,
        begin,
        "line 1\nline 2\n",
        "",
        /*exit_code*/ 0,
    );

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "shell output tool cell should update in place"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("Read output from shell `10555`"),
        "expected shell output tool call to render: {blob:?}"
    );
    assert!(
        blob.contains("line 1") && blob.contains("line 2"),
        "expected shell output tool output to render: {blob:?}"
    );
}

#[tokio::test]
async fn exec_history_shows_shell_management_tool_calls() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-list-shells",
        "list_shells",
        ExecCommandSource::UnifiedExecInteraction,
    );
    end_exec(
        &mut chat,
        begin,
        "1 active shell(s):\n- shell_id: 10555, runtime: 3s, command: sleep 60\n",
        "",
        /*exit_code*/ 0,
    );

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "list_shells cell should update in place"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("Listed active shells"),
        "expected list_shells tool call to render: {blob:?}"
    );
    assert!(
        blob.contains("shell_id: 10555"),
        "expected list_shells output to render: {blob:?}"
    );

    let begin = begin_exec_with_source(
        &mut chat,
        "call-stop-shell",
        "stop_shell 10555",
        ExecCommandSource::UnifiedExecInteraction,
    );
    end_exec(
        &mut chat,
        begin,
        "Stopped shell 10555 after 4s: sleep 60\n",
        "",
        /*exit_code*/ 0,
    );

    let blob = active_blob(&chat);
    assert!(
        blob.contains("Stopped shell `10555`"),
        "expected stop_shell tool call to render: {blob:?}"
    );
    assert!(
        blob.contains("Stopped shell 10555 after 4s"),
        "expected stop_shell output to render: {blob:?}"
    );
}

#[tokio::test]
async fn exec_history_shows_background_execute_as_running_without_output() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source_and_run_mode(
        &mut chat,
        "call-background",
        "i=1; while true; do echo $i; sleep 1; done",
        ExecCommandSource::UnifiedExecStartup,
        Some(ExecCommandRunMode::Background),
    );
    chat.handle_codex_event(Event {
        id: "sub-output".into(),
        msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id: "call-background".into(),
            stream: ExecOutputStream::Stdout,
            chunk: b"1\n2\n".to_vec(),
        }),
    });
    let live_blob = active_blob(&chat);
    assert!(
        !live_blob.contains("└ 1") && !live_blob.contains("└ 2"),
        "background execute should ignore live command output: {live_blob:?}"
    );
    end_exec(&mut chat, begin, "1\n2\n", "", /*exit_code*/ 0);

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "background execute cell should update in place"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains(
            "• Running i=1; while true; do … in background (Use down arrow to see details)"
        ),
        "expected background execute status: {blob:?}"
    );
    assert!(
        !blob.contains("Ran") && !blob.contains("└ 1") && !blob.contains("└ 2"),
        "background execute should not show Ran or command output: {blob:?}"
    );
}

#[tokio::test]
async fn background_execute_tool_response_keeps_footer_activity() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_unified_exec_startup(
        &mut chat,
        "call-background-footer",
        "12345",
        "while true; do echo hi; sleep 1; done",
    );
    assert_eq!(chat.unified_exec_processes.len(), 1);

    end_exec(&mut chat, begin, "hi\n", "", /*exit_code*/ 0);

    assert_eq!(
        chat.unified_exec_processes.len(),
        1,
        "initial background execute tool response must not remove the live shell from the footer"
    );
}

#[tokio::test]
async fn background_shell_interaction_output_updates_footer_details() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    begin_unified_exec_startup(&mut chat, "call-background", "12345", "npm run dev");
    terminal_interaction(&mut chat, "call-read-output", "12345", "");
    chat.handle_codex_event(Event {
        id: "read-output".into(),
        msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id: "call-read-output".into(),
            stream: ExecOutputStream::Stdout,
            chunk: b"ready\ncompiled\n".to_vec(),
        }),
    });

    chat.handle_key_event(KeyEvent::from(KeyCode::Down));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));
    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    let details = render_bottom_popup(&chat, /*width*/ 100);
    assert!(
        details.contains("output:"),
        "expected background shell output section, got {details:?}"
    );
    assert!(
        details.contains("ready") && details.contains("compiled"),
        "expected interaction output in footer details, got {details:?}"
    );
}

#[tokio::test]
async fn background_shell_exit_removes_footer_even_before_initial_end_is_handled() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin =
        begin_unified_exec_startup(&mut chat, "call-background-race", "12345", "printf done");
    assert_eq!(chat.unified_exec_processes.len(), 1);

    let mut initial = begin.clone();
    initial.run_mode = Some(ExecCommandRunMode::Background);
    let mut exit = begin;
    exit.run_mode = None;

    let initial_end = ExecCommandEndEvent {
        call_id: initial.call_id.clone(),
        process_id: initial.process_id.clone(),
        turn_id: initial.turn_id.clone(),
        command: initial.command.clone(),
        cwd: initial.cwd.clone(),
        parsed_cmd: initial.parsed_cmd.clone(),
        source: initial.source,
        interaction_input: initial.interaction_input,
        stdout: String::new(),
        stderr: String::new(),
        aggregated_output: String::new(),
        exit_code: 0,
        duration: std::time::Duration::from_millis(5),
        formatted_output: String::new(),
        status: CoreExecCommandStatus::Completed,
    };
    chat.track_unified_exec_process_end(&initial_end);
    assert_eq!(chat.unified_exec_processes.len(), 1);

    let exit_end = ExecCommandEndEvent {
        call_id: exit.call_id,
        process_id: exit.process_id,
        turn_id: exit.turn_id,
        command: exit.command,
        cwd: exit.cwd,
        parsed_cmd: exit.parsed_cmd,
        source: exit.source,
        interaction_input: exit.interaction_input,
        stdout: "done\n".to_string(),
        stderr: String::new(),
        aggregated_output: "done\n".to_string(),
        exit_code: 0,
        duration: std::time::Duration::from_millis(5),
        formatted_output: "done\n".to_string(),
        status: CoreExecCommandStatus::Completed,
    };
    chat.track_unified_exec_process_end(&exit_end);

    assert!(
        chat.running_commands.contains_key("call-background-race"),
        "test must keep running_commands stale to cover the race"
    );
    assert!(
        chat.unified_exec_processes.is_empty(),
        "real shell exit must remove the footer row even if the initial end is still queued"
    );
}

#[tokio::test]
async fn background_shell_exit_wakes_idle_model() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.on_task_started();

    let begin = begin_unified_exec_startup(
        &mut chat,
        "call-background-exit-idle",
        "12345",
        "printf done",
    );
    end_exec(&mut chat, begin.clone(), "", "", /*exit_code*/ 0);
    assert_no_submit_op(&mut op_rx);

    chat.handle_codex_event(Event {
        id: "turn-complete".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("started".to_string()),
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    end_exec(&mut chat, begin, "done\n", "", /*exit_code*/ 7);
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "background shell exit wake-up must not render a user-visible message"
    );

    let wake_up_text = "Background shell 12345 exited with code 7.\nCommand: printf done\n\nUse read_shell_output with shell_id 12345 if you need the final output.";
    match next_submit_op(&mut op_rx) {
        Op::UserTurn { items, .. } => assert_eq!(
            items,
            vec![UserInput::Text {
                text: wake_up_text.to_string(),
                text_elements: Vec::new(),
            }]
        ),
        other => panic!("expected shell exit wake-up user turn, got {other:?}"),
    }

    complete_user_message_for_inputs(
        &mut chat,
        "hidden-shell-exit-idle",
        vec![UserInput::Text {
            text: wake_up_text.to_string(),
            text_elements: Vec::new(),
        }],
    );
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "committed hidden shell exit message must stay out of history"
    );
}

#[tokio::test]
async fn background_shell_exit_notifies_running_model() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.on_task_started();

    let begin = begin_unified_exec_startup(
        &mut chat,
        "call-background-exit-running",
        "12345",
        "printf done",
    );
    end_exec(&mut chat, begin.clone(), "", "", /*exit_code*/ 0);
    assert_no_submit_op(&mut op_rx);

    end_exec(&mut chat, begin, "done\n", "", /*exit_code*/ 0);
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "background shell exit notification must not render a user-visible message"
    );

    let notification_text = "Background shell 12345 exited with code 0.\nCommand: printf done\n\nUse read_shell_output with shell_id 12345 if you need the final output.";
    match next_submit_op(&mut op_rx) {
        Op::UserTurn { items, .. } => assert_eq!(
            items,
            vec![UserInput::Text {
                text: notification_text.to_string(),
                text_elements: Vec::new(),
            }]
        ),
        other => panic!("expected shell exit notification user turn, got {other:?}"),
    }

    complete_user_message_for_inputs(
        &mut chat,
        "hidden-shell-exit-running",
        vec![UserInput::Text {
            text: notification_text.to_string(),
            text_elements: Vec::new(),
        }],
    );
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "committed hidden shell exit steer must stay out of history"
    );
}

#[tokio::test]
async fn exec_history_truncates_unified_exec_startup_command() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup-long",
        "echo 12345678901234567890",
        ExecCommandSource::UnifiedExecStartup,
    );
    end_exec(&mut chat, begin, "ok\n", "", /*exit_code*/ 0);

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "execute cell should update in place, not flush immediately"
    );
    let blob = active_blob(&chat);
    assert!(
        blob.contains("• Ran echo 123456789012345…"),
        "expected truncated execute command: {blob:?}"
    );
    assert!(
        !blob.contains("12345678901234567890"),
        "expected command tail to be omitted: {blob:?}"
    );
}

#[tokio::test]
async fn unified_exec_unknown_end_with_active_exploring_cell_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    begin_exec(&mut chat, "call-exploring", "cat /dev/null");
    let orphan =
        begin_unified_exec_startup(&mut chat, "call-orphan", "proc-1", "echo repro-marker");
    end_exec(
        &mut chat,
        orphan,
        "repro-marker\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    let history = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    let active = active_blob(&chat);
    let snapshot = format!("History:\n{history}\nActive:\n{active}");
    assert_chatwidget_snapshot!(
        "unified_exec_unknown_end_with_active_exploring_cell",
        snapshot
    );
}

#[tokio::test]
async fn unified_exec_end_after_task_complete_is_suppressed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();

    let begin = begin_exec_with_source(
        &mut chat,
        "call-startup",
        "echo unified exec startup",
        ExecCommandSource::UnifiedExecStartup,
    );
    drain_insert_history(&mut rx);

    chat.on_task_complete(/*last_agent_message*/ None, /*from_replay*/ false);
    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "expected unified exec end after task complete to be suppressed"
    );
}

#[tokio::test]
async fn unified_exec_interaction_after_task_complete_is_suppressed() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    chat.on_task_complete(/*last_agent_message*/ None, /*from_replay*/ false);

    chat.handle_codex_event(Event {
        id: "call-1".to_string(),
        msg: EventMsg::TerminalInteraction(TerminalInteractionEvent {
            call_id: "call-1".to_string(),
            process_id: "proc-1".to_string(),
            stdin: "ls\n".to_string(),
        }),
    });

    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "expected unified exec interaction after task complete to be suppressed"
    );
}

#[tokio::test]
async fn unified_exec_wait_after_final_agent_message_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }),
    });

    begin_unified_exec_startup(&mut chat, "call-wait", "proc-1", "cargo test -p codex-core");
    terminal_interaction(&mut chat, "call-wait-stdin", "proc-1", "");

    complete_assistant_message(&mut chat, "msg-1", "Final response.", /*phase*/ None);
    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("Final response.".into()),
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_wait_after_final_agent_message", combined);
}

#[tokio::test]
async fn unified_exec_wait_before_streamed_agent_message_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }),
    });

    begin_unified_exec_startup(
        &mut chat,
        "call-wait-stream",
        "proc-1",
        "cargo test -p codex-core",
    );
    terminal_interaction(&mut chat, "call-wait-stream-stdin", "proc-1", "");

    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
            delta: "Streaming response.".into(),
        }),
    });
    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_wait_before_streamed_agent_message", combined);
}

#[tokio::test]
async fn unified_exec_wait_status_header_updates_on_late_command_display() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    chat.unified_exec_processes.push(UnifiedExecProcessSummary {
        key: "proc-1".to_string(),
        call_id: "call-1".to_string(),
        command_display: "sleep 5".to_string(),
        run_mode: None,
        started_at: Instant::now(),
        recent_chunks: Vec::new(),
    });

    chat.on_terminal_interaction(TerminalInteractionEvent {
        call_id: "call-1".to_string(),
        process_id: "proc-1".to_string(),
        stdin: String::new(),
    });

    assert!(chat.active_cell.is_none());
    assert_eq!(
        chat.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("sleep 5"));
}

#[tokio::test]
async fn unified_exec_waiting_multiple_empty_snapshots() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-1", "proc-1", "just fix");

    terminal_interaction(&mut chat, "call-wait-1a", "proc-1", "");
    terminal_interaction(&mut chat, "call-wait-1b", "proc-1", "");
    assert_eq!(
        chat.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("just fix"));

    chat.handle_codex_event(Event {
        id: "turn-wait-1".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_waiting_multiple_empty_after", combined);
}

#[tokio::test]
async fn unified_exec_wait_status_renders_command_in_single_details_row_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(
        &mut chat,
        "call-wait-ui",
        "proc-ui",
        "cargo test -p codex-core -- --exact some::very::long::test::name",
    );

    terminal_interaction(&mut chat, "call-wait-ui-stdin", "proc-ui", "");

    let rendered = render_bottom_popup(&chat, /*width*/ 48);
    assert_chatwidget_snapshot!(
        "unified_exec_wait_status_renders_command_in_single_details_row",
        normalize_snapshot_paths(rendered)
    );
}

#[tokio::test]
async fn unified_exec_empty_then_non_empty_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-2", "proc-2", "just fix");

    terminal_interaction(&mut chat, "call-wait-2a", "proc-2", "");
    terminal_interaction(&mut chat, "call-wait-2b", "proc-2", "ls\n");

    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_empty_then_non_empty_after", combined);
}

#[tokio::test]
async fn unified_exec_non_empty_then_empty_snapshots() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_task_started();
    begin_unified_exec_startup(&mut chat, "call-wait-3", "proc-3", "just fix");

    terminal_interaction(&mut chat, "call-wait-3a", "proc-3", "pwd\n");
    terminal_interaction(&mut chat, "call-wait-3b", "proc-3", "");
    assert_eq!(
        chat.current_status.header,
        "Waiting for background terminal"
    );
    let status = chat
        .bottom_pane
        .status_widget()
        .expect("status indicator should be visible");
    assert_eq!(status.header(), "Waiting for background terminal");
    assert_eq!(status.details(), Some("just fix"));
    let pre_cells = drain_insert_history(&mut rx);
    let active_combined = pre_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    assert_chatwidget_snapshot!("unified_exec_non_empty_then_empty_active", active_combined);

    chat.handle_codex_event(Event {
        id: "turn-wait-3".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    let post_cells = drain_insert_history(&mut rx);
    let mut combined = pre_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    let post = post_cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<String>();
    if !combined.is_empty() && !post.is_empty() {
        combined.push('\n');
    }
    combined.push_str(&post);
    assert_chatwidget_snapshot!("unified_exec_non_empty_then_empty_after", combined);
}

#[tokio::test]
async fn view_image_tool_call_adds_history_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let image_path = chat.config.cwd.join("example.png");

    chat.handle_codex_event(Event {
        id: "sub-image".into(),
        msg: EventMsg::ViewImageToolCall(ViewImageToolCallEvent {
            call_id: "call-image".into(),
            path: image_path,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single history cell");
    let combined = lines_to_single_string(&cells[0]);
    assert_chatwidget_snapshot!("local_image_attachment_history_snapshot", combined);
}

#[tokio::test]
async fn image_generation_call_adds_history_cell() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.handle_codex_event(Event {
        id: "sub-image-generation".into(),
        msg: EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
            call_id: "call-image-generation".into(),
            status: "completed".into(),
            revised_prompt: Some("A tiny blue square".into()),
            result: "Zm9v".into(),
            saved_path: Some(test_path_buf("/tmp/ig-1.png").abs()),
        }),
    });

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1, "expected a single history cell");
    let platform_file_url = url::Url::from_file_path(test_path_buf("/tmp/ig-1.png"))
        .expect("test path should convert to file URL")
        .to_string();
    let combined =
        lines_to_single_string(&cells[0]).replace(&platform_file_url, "file:///tmp/ig-1.png");
    assert_chatwidget_snapshot!("image_generation_call_history_snapshot", combined);
}

#[tokio::test]
async fn exec_history_extends_previous_when_consecutive() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // 1) Start "ls -la" (List)
    let begin_ls = begin_exec(&mut chat, "call-ls", "ls -la");
    assert_chatwidget_snapshot!("exploring_step1_start_ls", active_blob(&chat));

    // 2) Finish "ls -la"
    end_exec(&mut chat, begin_ls, "", "", /*exit_code*/ 0);
    assert_chatwidget_snapshot!("exploring_step2_finish_ls", active_blob(&chat));

    // 3) Start "cat foo.txt" (Read)
    let begin_cat_foo = begin_exec(&mut chat, "call-cat-foo", "cat foo.txt");
    assert_chatwidget_snapshot!("exploring_step3_start_cat_foo", active_blob(&chat));

    // 4) Complete "cat foo.txt"
    end_exec(
        &mut chat,
        begin_cat_foo,
        "hello from foo",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step4_finish_cat_foo", active_blob(&chat));

    // 5) Start & complete "sed -n 100,200p foo.txt" (treated as Read of foo.txt)
    let begin_sed_range = begin_exec(&mut chat, "call-sed-range", "sed -n 100,200p foo.txt");
    end_exec(
        &mut chat,
        begin_sed_range,
        "chunk",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step5_finish_sed_range", active_blob(&chat));

    // 6) Start & complete "cat bar.txt"
    let begin_cat_bar = begin_exec(&mut chat, "call-cat-bar", "cat bar.txt");
    end_exec(
        &mut chat,
        begin_cat_bar,
        "hello from bar",
        "",
        /*exit_code*/ 0,
    );
    assert_chatwidget_snapshot!("exploring_step6_finish_cat_bar", active_blob(&chat));
}

#[tokio::test]
async fn user_shell_command_renders_output_not_exploring() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let begin_ls = begin_exec_with_source(
        &mut chat,
        "user-shell-ls",
        "ls",
        ExecCommandSource::UserShell,
    );
    end_exec(
        &mut chat,
        begin_ls,
        "file1\nfile2\n",
        "",
        /*exit_code*/ 0,
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(
        cells.len(),
        0,
        "completed user command should update the active history cell in place"
    );
    let blob = active_blob(&chat);
    assert_chatwidget_snapshot!("user_shell_ls_output", blob);
}

#[tokio::test]
async fn bang_shell_enter_while_task_running_submits_run_user_shell_command() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let conversation_id = ThreadId::new();
    let rollout_file = NamedTempFile::new().unwrap();
    let configured = codex_protocol::protocol::SessionConfiguredEvent {
        session_id: conversation_id,
        forked_from_id: None,
        thread_name: None,
        model: "test-model".to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: ApprovalsReviewer::User,
        permission_profile: PermissionProfile::read_only(),
        cwd: test_path_buf("/home/user/project").abs(),
        reasoning_effort: Some(ReasoningEffortConfig::default()),
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        network_proxy: None,
        rollout_path: Some(rollout_file.path().to_path_buf()),
    };
    chat.handle_codex_event(Event {
        id: "initial".into(),
        msg: EventMsg::SessionConfigured(configured),
    });
    drain_insert_history(&mut rx);
    while op_rx.try_recv().is_ok() {}
    chat.handle_codex_event(Event {
        id: "turn-start".into(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }),
    });

    chat.bottom_pane
        .set_composer_text("!echo hi".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    match op_rx.try_recv() {
        Ok(Op::RunUserShellCommand { command }) => assert_eq!(command, "echo hi"),
        other => panic!("expected RunUserShellCommand op, got {other:?}"),
    }
    assert_matches!(
        op_rx.try_recv(),
        Ok(Op::AddToHistory { text }) if text == "!echo hi"
    );
    assert_matches!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn user_message_during_user_shell_command_is_queued_not_steered() {
    let (mut chat, _rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.handle_codex_event(Event {
        id: "turn-start".into(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }),
    });
    let begin = begin_exec_with_source(
        &mut chat,
        "user-shell-sleep",
        "sleep 10",
        ExecCommandSource::UserShell,
    );

    assert!(chat.only_user_shell_commands_running());
    chat.bottom_pane
        .set_composer_text("hi".to_string(), Vec::new(), Vec::new());
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_matches!(op_rx.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(chat.queued_user_message_texts(), vec!["hi".to_string()]);

    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);
    chat.handle_codex_event(Event {
        id: "turn-complete".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("done".to_string()),
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    match next_submit_op(&mut op_rx) {
        Op::UserTurn { items, .. } => assert_eq!(
            items,
            vec![UserInput::Text {
                text: "hi".to_string(),
                text_elements: Vec::new(),
            }]
        ),
        other => panic!("expected queued user message after shell completion, got {other:?}"),
    }
    assert!(chat.queued_user_messages.is_empty());
}

#[tokio::test]
async fn disabled_slash_command_while_task_running_snapshot() {
    // Build a chat widget and simulate an active task
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.bottom_pane.set_task_running(/*running*/ true);

    // Dispatch a command that is unavailable while a task runs (e.g., /model)
    chat.dispatch_command(SlashCommand::Model);

    // Drain history and snapshot the rendered error line(s)
    let cells = drain_insert_history(&mut rx);
    assert!(
        !cells.is_empty(),
        "expected an error message history cell to be emitted",
    );
    let blob = lines_to_single_string(cells.last().unwrap());
    assert_chatwidget_snapshot!("disabled_slash_command_while_task_running_snapshot", blob);
}

//
// Snapshot test: command approval modal
//
// Synthesizes a Codex ExecApprovalRequest event to trigger the approval modal
// and snapshots the visual output using the ratatui TestBackend.
#[tokio::test]
async fn approval_modal_exec_snapshot() -> anyhow::Result<()> {
    // Build a chat widget with manual channels to avoid spawning the agent.
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    // Ensure policy allows surfacing approvals explicitly (not strictly required for direct event).
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;
    // Inject an exec approval request to display the approval modal.
    let ev = ExecApprovalRequestEvent {
        call_id: "call-approve-cmd".into(),
        approval_id: Some("call-approve-cmd".into()),
        turn_id: "turn-approve-cmd".into(),
        command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some(
            "this is a test reason such as one that would be produced by the model".into(),
        ),
        network_approval_context: None,
        proposed_execpolicy_amendment: Some(ExecPolicyAmendment::new(vec![
            "echo".into(),
            "hello".into(),
            "world".into(),
        ])),
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-approve".into(),
        msg: EventMsg::ExecApprovalRequest(ev),
    });
    // Render to a fixed-size test terminal and snapshot.
    // Call desired_height first and use that exact height for rendering.
    let width = 100;
    let height = chat.desired_height(width);
    let mut terminal =
        crate::custom_terminal::Terminal::with_options(VT100Backend::new(width, height))
            .expect("create terminal");
    let viewport = Rect::new(0, 0, width, height);
    terminal.set_viewport_area(viewport);

    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw approval modal");
    assert!(
        terminal
            .backend()
            .vt100()
            .screen()
            .contents()
            .contains("echo hello world")
    );
    assert_chatwidget_snapshot!(
        "approval_modal_exec",
        terminal.backend().vt100().screen().contents()
    );

    Ok(())
}

// Snapshot test: command approval modal without a reason
// Ensures spacing looks correct when no reason text is provided.
#[tokio::test]
async fn approval_modal_exec_without_reason_snapshot() -> anyhow::Result<()> {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;

    let ev = ExecApprovalRequestEvent {
        call_id: "call-approve-cmd-noreason".into(),
        approval_id: Some("call-approve-cmd-noreason".into()),
        turn_id: "turn-approve-cmd-noreason".into(),
        command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: None,
        network_approval_context: None,
        proposed_execpolicy_amendment: Some(ExecPolicyAmendment::new(vec![
            "echo".into(),
            "hello".into(),
            "world".into(),
        ])),
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-approve-noreason".into(),
        msg: EventMsg::ExecApprovalRequest(ev),
    });

    let width = 100;
    let height = chat.desired_height(width);
    let mut terminal =
        ratatui::Terminal::new(VT100Backend::new(width, height)).expect("create terminal");
    terminal.set_viewport_area(Rect::new(0, 0, width, height));
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw approval modal (no reason)");
    assert_chatwidget_snapshot!(
        "approval_modal_exec_no_reason",
        terminal.backend().vt100().screen().contents()
    );

    Ok(())
}

// Snapshot test: approval modal with a proposed execpolicy prefix that is multi-line;
// we should not offer adding it to execpolicy.
#[tokio::test]
async fn approval_modal_exec_multiline_prefix_hides_execpolicy_option_snapshot()
-> anyhow::Result<()> {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;

    let script = "python - <<'PY'\nprint('hello')\nPY".to_string();
    let command = vec!["bash".into(), "-lc".into(), script];
    let ev = ExecApprovalRequestEvent {
        call_id: "call-approve-cmd-multiline-trunc".into(),
        approval_id: Some("call-approve-cmd-multiline-trunc".into()),
        turn_id: "turn-approve-cmd-multiline-trunc".into(),
        command: command.clone(),
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: None,
        network_approval_context: None,
        proposed_execpolicy_amendment: Some(ExecPolicyAmendment::new(command)),
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
        parsed_cmd: vec![],
    };
    chat.handle_codex_event(Event {
        id: "sub-approve-multiline-trunc".into(),
        msg: EventMsg::ExecApprovalRequest(ev),
    });

    let width = 100;
    let height = chat.desired_height(width);
    let mut terminal =
        ratatui::Terminal::new(VT100Backend::new(width, height)).expect("create terminal");
    terminal.set_viewport_area(Rect::new(0, 0, width, height));
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw approval modal (multiline prefix)");
    let contents = terminal.backend().vt100().screen().contents();
    assert!(!contents.contains("don't ask again"));
    assert_chatwidget_snapshot!(
        "approval_modal_exec_multiline_prefix_no_execpolicy",
        contents
    );

    Ok(())
}

// Snapshot test: patch approval modal
#[tokio::test]
async fn approval_modal_patch_snapshot() -> anyhow::Result<()> {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;

    // Build a small changeset and a reason/grant_root to exercise the prompt text.
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("README.md"),
        FileChange::Add {
            content: "hello\nworld\n".into(),
        },
    );
    let ev = ApplyPatchApprovalRequestEvent {
        call_id: "call-approve-patch".into(),
        turn_id: "turn-approve-patch".into(),
        changes,
        reason: Some("The model wants to apply changes".into()),
        grant_root: Some(PathBuf::from("/tmp")),
    };
    chat.handle_codex_event(Event {
        id: "sub-approve-patch".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ev),
    });

    // Render at the widget's desired height and snapshot.
    let height = chat.desired_height(/*width*/ 80);
    let mut terminal =
        ratatui::Terminal::new(VT100Backend::new(/*width*/ 80, height)).expect("create terminal");
    terminal.set_viewport_area(Rect::new(0, 0, 80, height));
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw patch approval modal");
    let contents = terminal.backend().vt100().screen().contents();
    assert!(!contents.contains("$ apply_patch"));
    assert_chatwidget_snapshot!("approval_modal_patch", contents);

    Ok(())
}

#[tokio::test]
async fn interrupt_preserves_unified_exec_processes() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    begin_unified_exec_startup(&mut chat, "call-1", "process-1", "sleep 5");
    begin_unified_exec_startup(&mut chat, "call-2", "process-2", "sleep 6");
    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
            completed_at: None,
            duration_ms: None,
        }),
    });

    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.add_ps_output();
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("Background terminals"),
        "expected /ps to remain available after interrupt; got {combined:?}"
    );
    assert!(
        combined.contains("sleep 5") && combined.contains("sleep 6"),
        "expected /ps to list running unified exec processes; got {combined:?}"
    );

    let _ = drain_insert_history(&mut rx);
}

#[tokio::test]
async fn interrupt_preserves_unified_exec_wait_streak_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }),
    });

    let begin = begin_unified_exec_startup(&mut chat, "call-1", "process-1", "just fix");
    terminal_interaction(&mut chat, "call-1a", "process-1", "");

    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
            completed_at: None,
            duration_ms: None,
        }),
    });

    end_exec(&mut chat, begin, "", "", /*exit_code*/ 0);
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    let snapshot = format!("cells={}\n{combined}", cells.len());
    assert_chatwidget_snapshot!("interrupt_preserves_unified_exec_wait_streak", snapshot);
}

#[tokio::test]
async fn turn_complete_keeps_unified_exec_processes() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    begin_unified_exec_startup(&mut chat, "call-1", "process-1", "sleep 5");
    begin_unified_exec_startup(&mut chat, "call-2", "process-2", "sleep 6");
    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.handle_codex_event(Event {
        id: "turn-1".into(),
        msg: EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
    });

    assert_eq!(chat.unified_exec_processes.len(), 2);

    chat.add_ps_output();
    let cells = drain_insert_history(&mut rx);
    let combined = cells
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("Background terminals"),
        "expected /ps to remain available after turn complete; got {combined:?}"
    );
    assert!(
        combined.contains("sleep 5") && combined.contains("sleep 6"),
        "expected /ps to list running unified exec processes; got {combined:?}"
    );

    let _ = drain_insert_history(&mut rx);
}

#[tokio::test]
async fn apply_patch_events_emit_history_cells() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // 1) Approval request -> proposed patch summary cell
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    let ev = ApplyPatchApprovalRequestEvent {
        call_id: "c1".into(),
        turn_id: "turn-c1".into(),
        changes,
        reason: None,
        grant_root: None,
    };
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ev),
    });
    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "expected approval request to surface via modal without emitting history cells"
    );

    // 2) Begin apply -> per-file apply block cell (no global header)
    let mut changes2 = HashMap::new();
    changes2.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    let begin = PatchApplyBeginEvent {
        call_id: "c1".into(),
        turn_id: "turn-c1".into(),
        auto_approved: true,
        changes: changes2,
    };
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::PatchApplyBegin(begin),
    });
    let cells = drain_insert_history(&mut rx);
    assert!(!cells.is_empty(), "expected apply block cell to be sent");
    let blob = lines_to_single_string(cells.last().unwrap());
    assert!(
        blob.contains("Added foo.txt") || blob.contains("Edited foo.txt"),
        "expected single-file header with filename (Added/Edited): {blob:?}"
    );

    // 3) End apply success -> success cell
    let mut end_changes = HashMap::new();
    end_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    let end = PatchApplyEndEvent {
        call_id: "c1".into(),
        turn_id: "turn-c1".into(),
        stdout: "ok\n".into(),
        stderr: String::new(),
        success: true,
        changes: end_changes,
        status: CorePatchApplyStatus::Completed,
    };
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::PatchApplyEnd(end),
    });
    let cells = drain_insert_history(&mut rx);
    assert!(
        cells.is_empty(),
        "no success cell should be emitted anymore"
    );
}

#[tokio::test]
async fn apply_patch_manual_approval_adjusts_header() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let mut proposed_changes = HashMap::new();
    proposed_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id: "c1".into(),
            turn_id: "turn-c1".into(),
            changes: proposed_changes,
            reason: None,
            grant_root: None,
        }),
    });
    drain_insert_history(&mut rx);

    let mut apply_changes = HashMap::new();
    apply_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id: "c1".into(),
            turn_id: "turn-c1".into(),
            auto_approved: false,
            changes: apply_changes,
        }),
    });

    let cells = drain_insert_history(&mut rx);
    assert!(!cells.is_empty(), "expected apply block cell to be sent");
    let blob = lines_to_single_string(cells.last().unwrap());
    assert!(
        blob.contains("Added foo.txt") || blob.contains("Edited foo.txt"),
        "expected apply summary header for foo.txt: {blob:?}"
    );
}

#[tokio::test]
async fn apply_patch_manual_flow_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let mut proposed_changes = HashMap::new();
    proposed_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id: "c1".into(),
            turn_id: "turn-c1".into(),
            changes: proposed_changes,
            reason: Some("Manual review required".into()),
            grant_root: None,
        }),
    });
    let history_before_apply = drain_insert_history(&mut rx);
    assert!(
        history_before_apply.is_empty(),
        "expected approval modal to defer history emission"
    );

    let mut apply_changes = HashMap::new();
    apply_changes.insert(
        PathBuf::from("foo.txt"),
        FileChange::Add {
            content: "hello\n".to_string(),
        },
    );
    chat.handle_codex_event(Event {
        id: "s1".into(),
        msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id: "c1".into(),
            turn_id: "turn-c1".into(),
            auto_approved: false,
            changes: apply_changes,
        }),
    });
    let approved_lines = drain_insert_history(&mut rx)
        .pop()
        .expect("approved patch cell");

    assert_chatwidget_snapshot!(
        "apply_patch_manual_flow_history_approved",
        lines_to_single_string(&approved_lines)
    );
}

#[tokio::test]
async fn apply_patch_approval_sends_op_with_call_id() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    // Simulate receiving an approval request with a distinct event id and call id.
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("file.rs"),
        FileChange::Add {
            content: "fn main(){}\n".into(),
        },
    );
    let ev = ApplyPatchApprovalRequestEvent {
        call_id: "call-999".into(),
        turn_id: "turn-999".into(),
        changes,
        reason: None,
        grant_root: None,
    };
    chat.handle_codex_event(Event {
        id: "sub-123".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ev),
    });

    // Approve via key press 'y'
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    // Expect a thread-scoped PatchApproval op carrying the call id.
    let mut found = false;
    while let Ok(app_ev) = rx.try_recv() {
        if let AppEvent::SubmitThreadOp {
            op: Op::PatchApproval { id, decision },
            ..
        } = app_ev
        {
            assert_eq!(id, "call-999");
            assert_matches!(decision, codex_protocol::protocol::ReviewDecision::Approved);
            found = true;
            break;
        }
    }
    assert!(found, "expected PatchApproval op to be sent");
}

#[tokio::test]
async fn apply_patch_full_flow_integration_like() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // 1) Backend requests approval
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("pkg.rs"),
        FileChange::Add { content: "".into() },
    );
    chat.handle_codex_event(Event {
        id: "sub-xyz".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id: "call-1".into(),
            turn_id: "turn-call-1".into(),
            changes,
            reason: None,
            grant_root: None,
        }),
    });

    // 2) User approves via 'y' and App receives a thread-scoped op
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let mut maybe_op: Option<Op> = None;
    while let Ok(app_ev) = rx.try_recv() {
        if let AppEvent::SubmitThreadOp { op, .. } = app_ev {
            maybe_op = Some(op);
            break;
        }
    }
    let op = maybe_op.expect("expected thread-scoped op after key press");

    // 3) App forwards to widget.submit_op, which pushes onto codex_op_tx
    chat.submit_op(op);
    let forwarded = op_rx
        .try_recv()
        .expect("expected op forwarded to codex channel");
    match forwarded {
        Op::PatchApproval { id, decision } => {
            assert_eq!(id, "call-1");
            assert_matches!(decision, codex_protocol::protocol::ReviewDecision::Approved);
        }
        other => panic!("unexpected op forwarded: {other:?}"),
    }

    // 4) Simulate patch begin/end events from backend; ensure history cells are emitted
    let mut changes2 = HashMap::new();
    changes2.insert(
        PathBuf::from("pkg.rs"),
        FileChange::Add { content: "".into() },
    );
    chat.handle_codex_event(Event {
        id: "sub-xyz".into(),
        msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
            call_id: "call-1".into(),
            turn_id: "turn-call-1".into(),
            auto_approved: false,
            changes: changes2,
        }),
    });
    let mut end_changes = HashMap::new();
    end_changes.insert(
        PathBuf::from("pkg.rs"),
        FileChange::Add { content: "".into() },
    );
    chat.handle_codex_event(Event {
        id: "sub-xyz".into(),
        msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id: "call-1".into(),
            turn_id: "turn-call-1".into(),
            stdout: String::from("ok"),
            stderr: String::new(),
            success: true,
            changes: end_changes,
            status: CorePatchApplyStatus::Completed,
        }),
    });
}

#[tokio::test]
async fn apply_patch_untrusted_shows_approval_modal() -> anyhow::Result<()> {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    // Ensure approval policy is untrusted (OnRequest)
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;

    // Simulate a patch approval request from backend
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("a.rs"),
        FileChange::Add { content: "".into() },
    );
    chat.handle_codex_event(Event {
        id: "sub-1".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id: "call-1".into(),
            turn_id: "turn-call-1".into(),
            changes,
            reason: None,
            grant_root: None,
        }),
    });

    // Render and ensure the approval modal title is present
    let area = Rect::new(0, 0, 80, 12);
    let mut buf = Buffer::empty(area);
    chat.render(area, &mut buf);

    let mut contains_title = false;
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        if row.contains("Would you like to make the following edits?") {
            contains_title = true;
            break;
        }
    }
    assert!(
        contains_title,
        "expected approval modal to be visible with title 'Would you like to make the following edits?'"
    );

    Ok(())
}

#[tokio::test]
async fn apply_patch_request_omits_diff_summary_from_modal() -> anyhow::Result<()> {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Ensure we are in OnRequest so an approval is surfaced
    chat.config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)?;

    // Simulate backend asking to apply a patch adding two lines to README.md
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("README.md"),
        FileChange::Add {
            // Two lines (no trailing empty line counted)
            content: "line one\nline two\n".into(),
        },
    );
    chat.handle_codex_event(Event {
        id: "sub-apply".into(),
        msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id: "call-apply".into(),
            turn_id: "turn-apply".into(),
            changes,
            reason: None,
            grant_root: None,
        }),
    });

    assert!(
        drain_insert_history(&mut rx).is_empty(),
        "expected approval request to render via modal instead of history"
    );

    let area = Rect::new(0, 0, 80, chat.desired_height(/*width*/ 80));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buf);
    let mut contents = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            contents.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        contents.push('\n');
    }
    assert!(!contents.contains("README.md (+2 -0)"));
    assert!(!contents.contains("+line one"));
    assert!(!contents.contains("+line two"));

    Ok(())
}
