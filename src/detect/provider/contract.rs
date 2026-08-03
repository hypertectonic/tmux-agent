use super::ProviderDetection;
use crate::model::{AgentState, EvidenceSource};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    name: String,
    title: String,
    screen: String,
    state: AgentState,
    source: EvidenceSource,
    signal: String,
    scope: String,
    definitive: bool,
    inferred: bool,
    preserve_previous: bool,
}

fn verify(fixture: &str, detector: impl Fn(&str, &str) -> ProviderDetection) {
    let cases: Vec<Case> = serde_json::from_str(fixture).expect("valid detection fixture");
    for case in cases {
        let actual = detector(&case.title, &case.screen);
        assert_eq!(actual.state, case.state, "{} state", case.name);
        assert_eq!(actual.source, case.source, "{} source", case.name);
        assert_eq!(actual.signal, case.signal, "{} signal", case.name);
        assert_eq!(actual.scope, case.scope, "{} scope", case.name);
        assert_eq!(
            actual.definitive, case.definitive,
            "{} definitive",
            case.name
        );
        assert_eq!(actual.inferred, case.inferred, "{} inferred", case.name);
        assert_eq!(
            actual.preserve_previous, case.preserve_previous,
            "{} preservation",
            case.name
        );
    }
}

#[test]
fn codex_visible_behavior_contract() {
    verify(
        include_str!("../../../tests/fixtures/detection/codex.json"),
        super::codex::detect,
    );
}

#[test]
fn claude_visible_behavior_contract() {
    verify(
        include_str!("../../../tests/fixtures/detection/claude.json"),
        super::claude::detect,
    );
}

#[test]
fn grok_visible_behavior_contract() {
    verify(
        include_str!("../../../tests/fixtures/detection/grok.json"),
        super::grok::detect,
    );
}

#[test]
fn opencode_visible_behavior_contract() {
    verify(
        include_str!("../../../tests/fixtures/detection/opencode.json"),
        super::opencode::detect,
    );
}
