//! Slash command inventory shared by the server and composer.

use crate::AgentBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Compact,
    Clear,
    New,
    Model,
    Effort,
    Escalate,
    Archive,
    Pin,
    Goal,
    Pace,
    Join,
    Stop,
    Help,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub command: Command,
    pub name: &'static str,
    pub arguments: &'static str,
    pub description: &'static str,
    pub backends: &'static [AgentBackend],
}

impl Spec {
    pub fn available(self, backend: AgentBackend) -> bool {
        self.backends.contains(&backend)
    }

    pub fn usage(self) -> String {
        format!("/{}{}", self.name, self.arguments)
    }
}

macro_rules! spec {
    ($command:ident, $name:literal, $arguments:literal, $description:literal) => {
        Spec {
            command: Command::$command,
            name: $name,
            arguments: $arguments,
            description: $description,
            backends: &[AgentBackend::Claude, AgentBackend::Codex],
        }
    };
}

pub const COMMANDS: &[Spec] = &[
    spec!(Compact, "compact", " [instructions]", "command-compact"),
    spec!(Clear, "clear", "", "command-clear"),
    spec!(New, "new", "", "command-new"),
    spec!(Model, "model", " [name]", "command-model"),
    spec!(Effort, "effort", " [level]", "command-effort"),
    spec!(Escalate, "escalate", " <message>", "command-escalate"),
    spec!(Archive, "archive", "", "command-archive"),
    spec!(Pin, "pin", "", "command-pin"),
    spec!(Goal, "goal", " [text]", "command-goal"),
    spec!(Pace, "pace", " [full|normal|economy]", "command-pace"),
    spec!(Join, "join", "", "command-join"),
    spec!(Stop, "stop", "", "command-stop"),
    spec!(Help, "help", "", "command-help"),
];

/// Item prefix for a verbatim command submitted by the user.
pub const ECHO: &str = "slash-command:";

pub fn is_command(text: &str) -> bool {
    text.starts_with('/') && !text.starts_with("//")
}
