use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundShellNotification {
    pub(crate) shell_id: i32,
    pub(crate) command: String,
    pub(crate) exit_code: i32,
}

impl BackgroundShellNotification {
    pub(crate) fn new(shell_id: i32, command: impl Into<String>, exit_code: i32) -> Self {
        Self {
            shell_id,
            command: command.into(),
            exit_code,
        }
    }
}

impl ContextualUserFragment for BackgroundShellNotification {
    const ROLE: &'static str = "user";
    const START_MARKER: &'static str = "<background_shell_notification>";
    const END_MARKER: &'static str = "</background_shell_notification>";

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "shell_id": self.shell_id,
                "command": self.command,
                "exit_code": self.exit_code,
                "hint": format!(
                    "Use read_shell_output with shell_id {} if you need the final output.",
                    self.shell_id
                ),
            })
        )
    }
}
