#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    pub fn parse_cli_label(value: &str) -> Option<Self> {
        [Self::ClaudeCode, Self::Codex]
            .into_iter()
            .find(|harness| harness.cli_label() == value)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_preserves_explicit_boundary_labels() {
        assert_eq!(Harness::ClaudeCode.capture_label(), "claude-code");
        assert_eq!(Harness::ClaudeCode.cli_label(), "claude");
        assert_eq!(Harness::ClaudeCode.ledger_label(), "claude");
        assert_eq!(
            Harness::parse_cli_label("claude"),
            Some(Harness::ClaudeCode)
        );
        assert_eq!(Harness::parse_ledger_label("codex"), Some(Harness::Codex));
        assert_eq!(Harness::parse_cli_label("claude-code"), None);
        assert_eq!(Harness::parse_ledger_label("claude-code"), None);
    }
}
