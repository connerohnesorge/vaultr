#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    pub fn parse_ledger_label(value: &str) -> Option<Self> {
        [Self::ClaudeCode, Self::Codex]
            .into_iter()
            .find(|harness| harness.ledger_label() == value)
    }

    pub fn cli_label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn ledger_label(self) -> &'static str {
        self.cli_label()
    }

    pub fn capture_label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// Which agent CLI a Plant job dispatches into a Herdr pane.
///
/// Deliberately NOT `Harness`. `Harness` is the *capture* identity — the wire
/// dialect an adapter speaks and the ledger stream a learn pass leases against.
/// `AgentCli` is the *launch* identity — which binary a pane runs. They stopped
/// being the same thing when prime-agent arrived: it is its own CLI but speaks
/// the Codex Responses dialect, so it captures as `codex` while launching as
/// `prime`. Folding it into `Harness` would have forced a third arm through
/// every proxy adapter and sweep match for a harness that has no adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentCli {
    ClaudeCode,
    Codex,
    Prime,
}

impl AgentCli {
    pub fn parse_cli_label(value: &str) -> Option<Self> {
        [Self::ClaudeCode, Self::Codex, Self::Prime]
            .into_iter()
            .find(|cli| cli.cli_label() == value)
    }

    pub fn cli_label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Prime => "prime",
        }
    }

    /// The `agent` value Herdr reports for a pane running this CLI.
    pub fn herdr_agent(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Prime => "prime-agent",
        }
    }

    /// Whether Herdr's reported pane `agent` is one Plant knows how to launch.
    ///
    /// Pane gates ask this instead of testing a hardcoded `"claude" | "codex"`
    /// pair. That literal silently excluded prime-agent panes from the
    /// agent-start subscription, so a prime run was never observed reaching
    /// `working` and sat until its timeout.
    pub fn is_known_herdr_agent(agent: Option<&str>) -> bool {
        agent.is_some_and(|agent| Self::from_herdr_agent(agent).is_some())
    }

    pub fn from_herdr_agent(value: &str) -> Option<Self> {
        [Self::ClaudeCode, Self::Codex, Self::Prime]
            .into_iter()
            .find(|cli| cli.herdr_agent() == value)
    }
}

/// Reasoning effort, held as one value and rendered per CLI.
///
/// Each agent CLI spells this differently (`-c model_reasoning_effort=` vs
/// `--thinking`), so jobs used to pass raw `--args` strings and a typo silently
/// produced a launch line the agent ignored. Parsing it here makes a bad effort
/// a startup error instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_cli_separates_launch_identity_from_capture_identity() {
        // `--cli prime` is the launch label; "prime-agent" is what Herdr calls the
        // pane. Keeping them distinct is the whole reason this enum is not Harness.
        assert_eq!(AgentCli::parse_cli_label("prime"), Some(AgentCli::Prime));
        assert_eq!(AgentCli::parse_cli_label("prime-agent"), None);
        assert_eq!(AgentCli::Prime.herdr_agent(), "prime-agent");
        assert_eq!(
            AgentCli::from_herdr_agent("prime-agent"),
            Some(AgentCli::Prime)
        );
    }

    #[test]
    fn pane_gates_recognize_every_launchable_agent() {
        for agent in ["claude", "codex", "prime-agent"] {
            assert!(
                AgentCli::is_known_herdr_agent(Some(agent)),
                "{agent} pane would be gated out"
            );
        }
        // an unrecognized or agentless pane is still not a plant agent pane
        assert!(!AgentCli::is_known_herdr_agent(Some("cursor")));
        assert!(!AgentCli::is_known_herdr_agent(Some("prime")));
        assert!(!AgentCli::is_known_herdr_agent(None));
    }

    #[test]
    fn effort_rejects_values_no_agent_accepts() {
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("maximum"), None);
        assert_eq!(Effort::parse("MAX"), None);
        assert_eq!(Effort::Max.label(), "max");
    }

    #[test]
    fn harness_preserves_explicit_boundary_labels() {
        assert_eq!(Harness::ClaudeCode.capture_label(), "claude-code");
        assert_eq!(Harness::ClaudeCode.cli_label(), "claude");
        assert_eq!(Harness::ClaudeCode.ledger_label(), "claude");
        assert_eq!(
            Harness::parse_ledger_label("claude"),
            Some(Harness::ClaudeCode)
        );
        assert_eq!(Harness::parse_ledger_label("codex"), Some(Harness::Codex));
        assert_eq!(Harness::parse_ledger_label("claude-code"), None);
    }
}
