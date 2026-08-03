mod claude;
mod codex;
#[cfg(test)]
mod contract;
mod grok;
mod opencode;
mod screen;

use crate::model::{AgentState, DetectionDetails, EvidenceSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProviderDetection {
    pub state: AgentState,
    pub source: EvidenceSource,
    pub signal: &'static str,
    pub scope: &'static str,
    pub definitive: bool,
    pub inferred: bool,
    pub preserve_previous: bool,
}

impl ProviderDetection {
    fn observed(
        state: AgentState,
        source: EvidenceSource,
        signal: &'static str,
        scope: &'static str,
    ) -> Self {
        Self {
            state,
            source,
            signal,
            scope,
            definitive: true,
            inferred: false,
            preserve_previous: false,
        }
    }

    fn from_screen(state: AgentState, signal: &'static str, scope: &'static str) -> Self {
        Self::observed(state, EvidenceSource::Screen, signal, scope)
    }

    fn from_title(state: AgentState, signal: &'static str) -> Self {
        Self::observed(state, EvidenceSource::Title, signal, "terminal_title")
    }

    fn inferred_idle(detector: &'static str) -> Self {
        Self {
            state: AgentState::Idle,
            source: EvidenceSource::Process,
            signal: detector,
            scope: "foreground_process",
            definitive: false,
            inferred: true,
            preserve_previous: false,
        }
    }

    fn preserve(signal: &'static str, scope: &'static str) -> Self {
        Self {
            state: AgentState::Unknown,
            source: EvidenceSource::Screen,
            signal,
            scope,
            definitive: false,
            inferred: false,
            preserve_previous: true,
        }
    }

    pub(super) fn details(self, detector: &str) -> DetectionDetails {
        DetectionDetails {
            engine: "provider".into(),
            detector: Some(detector.into()),
            observed_state: self.state,
            signal: Some(self.signal.into()),
            scope: Some(self.scope.into()),
            definitive: self.definitive,
            inferred: self.inferred,
            preserve_previous: self.preserve_previous,
            transition: None,
        }
    }
}

pub(super) fn detect(agent: &str, title: &str, screen: &str) -> Option<ProviderDetection> {
    match agent {
        "Codex" => Some(codex::detect(title, screen)),
        "Claude" => Some(claude::detect(title, screen)),
        "Grok" => Some(grok::detect(title, screen)),
        "OpenCode" => Some(opencode::detect(title, screen)),
        _ => None,
    }
}
