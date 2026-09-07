use super::*;
use crate::model::AgentOrigin;

fn attention(state: AgentState, seen: bool) -> Attention {
    match state {
        AgentState::Blocked => Attention::Blocked,
        AgentState::Working => Attention::Working,
        AgentState::Idle if !seen => Attention::Done,
        AgentState::Idle => Attention::Idle,
        AgentState::Unknown => Attention::Unknown,
    }
}

fn old(state: AgentState, seen: bool) -> AgentRecord {
    AgentRecord {
        id: "host/default/%1".into(),
        host: "host".into(),
        server: "default".into(),
        pane_id: "%1".into(),
        pane_pid: 1,
        session_id: "$1".into(),
        session_name: "main".into(),
        window_id: "@1".into(),
        window_index: 1,
        window_name: "work".into(),
        pane_index: 0,
        agent: "Codex".into(),
        state,
        attention: attention(state, seen),
        source: EvidenceSource::Screen,
        title: String::new(),
        label: None,
        cwd: String::new(),
        visible: false,
        seen,
        changed_at_ms: 1,
        origin: AgentOrigin::Tmux,
        terminal: None,
        remote_alias: None,
        ssh_connection: None,
        session_connections: None,
        focus_target: None,
        goal: None,
        subagent: None,
        detection: None,
    }
}

fn link_codex_thread_subagents(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    root_rollouts: &HashMap<String, RootRollout>,
    record_thread_ids: &HashMap<String, String>,
    record_starts: &HashMap<String, (String, u64)>,
    previous: &HashMap<String, AgentRecord>,
    location: (&str, &str),
) {
    let mut ownership = CodexOwnership::new(previous, location.0, location.1);
    let mut record_thread_ids = record_thread_ids.clone();
    ownership.reconcile_after_process_linking(ReconciliationFrame {
        records,
        record_thread_ids: &mut record_thread_ids,
        record_starts,
        previous,
        threads,
        root_rollouts,
        recovered_root_threads: &HashMap::new(),
    });
}

fn codex_thread(finished_at_ms: Option<u64>) -> ThreadRollout {
    ThreadRollout {
        thread_id: "01800000-0000-7000-8000-000000000002".into(),
        parent_thread_id: "01800000-0000-7000-8000-000000000001".into(),
        cwd: "/work".into(),
        started_at_ms: 5_000,
        finished_at_ms,
        name: Some("Worker".into()),
        agent_path: None,
        depth: Some(1),
        process_backed: true,
    }
}

#[test]
fn resumed_codex_thread_links_by_exact_id_despite_wrapper_cwd() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/wrapper-home".into();
    parent.origin = AgentOrigin::Terminal;
    let parent_id = parent.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent)]);
    let thread_ids = HashMap::from([(
        parent_id.clone(),
        "01800000-0000-7000-8000-000000000001".into(),
    )]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    let child = &records["host/codex-thread/01800000-0000-7000-8000-000000000002"];
    assert_eq!(child.attention, Attention::Working);
    assert_eq!(
        child
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(parent_id.as_str())
    );
    assert_eq!(
        child
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.name.as_deref()),
        Some("Worker")
    );
}

#[test]
fn agent_path_nests_process_child_under_in_process_thread() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_500,
        finished_at_ms: None,
        name: Some("review".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
    let mut thread = codex_thread(None);
    thread.agent_path = Some("/root/codex_review".into());
    thread.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[thread],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    let synthetic_id = "host/codex-thread/01800000-0000-7000-8000-000000000002";
    let worker = &records[synthetic_id];
    assert_eq!(
        worker
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some("host/run/main")
    );
    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(synthetic_id)
    );
}

#[test]
fn task_specific_agent_path_nests_role_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_500,
        finished_at_ms: None,
        name: Some("review".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
    let mut thread = codex_thread(None);
    thread.agent_path = Some("/root/final_memory_recovery_review".into());
    thread.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[thread],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    let synthetic_id = "host/codex-thread/01800000-0000-7000-8000-000000000002";
    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(synthetic_id)
    );
}

#[test]
fn task_specific_agent_path_does_not_reparent_older_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 4_500,
        finished_at_ms: None,
        name: Some("review".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
    let thread_ids = HashMap::from([(
        parent_id.clone(),
        "01800000-0000-7000-8000-000000000001".into(),
    )]);
    let mut thread = codex_thread(None);
    thread.agent_path = Some("/root/final_memory_recovery_review".into());
    thread.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[thread],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(parent_id.as_str())
    );
}

#[test]
fn completed_role_thread_does_not_claim_later_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 6_600,
        finished_at_ms: None,
        name: Some("review".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
    let thread_ids = HashMap::from([(parent_id, "root-thread".into())]);
    let mut completed = codex_thread(Some(6_000));
    completed.thread_id = "completed-review".into();
    completed.parent_thread_id = "root-thread".into();
    completed.agent_path = Some("/root/first_task_review".into());
    completed.process_backed = false;
    let mut active = codex_thread(None);
    active.thread_id = "active-review".into();
    active.parent_thread_id = "root-thread".into();
    active.started_at_ms = 6_500;
    active.agent_path = Some("/root/second_task_review".into());
    active.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[completed, active],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some("host/codex-thread/active-review")
    );
}

#[test]
fn latest_overlapping_role_thread_claims_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 6_600,
        finished_at_ms: None,
        name: Some("review".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (review_id.clone(), review)]);
    let thread_ids = HashMap::from([(parent_id, "root-thread".into())]);
    let mut first = codex_thread(None);
    first.thread_id = "first-review".into();
    first.parent_thread_id = "root-thread".into();
    first.agent_path = Some("/root/first_task_review".into());
    first.process_backed = false;
    let mut second = codex_thread(None);
    second.thread_id = "second-review".into();
    second.parent_thread_id = "root-thread".into();
    second.started_at_ms = 6_500;
    second.agent_path = Some("/root/second_task_review".into());
    second.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[first, second],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some("host/codex-thread/second-review")
    );
}

#[test]
fn codex_thread_fallback_refuses_ambiguous_same_cwd_parents() {
    let mut first = old(AgentState::Working, true);
    first.id = "host/default/%1".into();
    first.cwd = "/work".into();
    let mut second = old(AgentState::Working, true);
    second.id = "host/default/%2".into();
    second.cwd = "/work".into();
    let mut records = HashMap::from([(first.id.clone(), first), (second.id.clone(), second)]);
    let record_starts = HashMap::from([
        ("host/default/%1".into(), ("Codex:1".into(), 1_000)),
        ("host/default/%2".into(), ("Codex:2".into(), 1_000)),
    ]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 2);
    assert!(records.values().all(|record| record.subagent.is_none()));
}

#[test]
fn codex_thread_fallback_rejects_a_different_known_root_session() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/default/%1".into();
    parent.cwd = "/work".into();
    let mut records = HashMap::from([(parent.id.clone(), parent)]);
    let record_starts = HashMap::from([("host/default/%1".into(), ("Codex:1".into(), 50_000))]);
    let root_rollouts = HashMap::from([(
        "01800000-0000-7000-8000-000000000001".into(),
        RootRollout {
            cwd: "/work".into(),
            started_at_ms: 1_000,
        },
    )]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &root_rollouts,
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 1);
    assert!(records.values().all(|record| record.subagent.is_none()));
}

#[test]
fn codex_thread_fallback_matches_a_fresh_known_root_session_by_start_time() {
    let mut parent = old(AgentState::Idle, true);
    parent.id = "host/default/%1".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent)]);
    let record_starts = HashMap::from([(parent_id.clone(), ("Codex:1".into(), 1_500))]);
    let root_rollouts = HashMap::from([(
        "01800000-0000-7000-8000-000000000001".into(),
        RootRollout {
            cwd: "/work".into(),
            started_at_ms: 1_000,
        },
    )]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &root_rollouts,
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(parent_id.as_str())
    );
}

#[test]
fn codex_thread_fallback_matches_a_known_root_by_cwd_and_start_time() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/default/%1".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent)]);
    let record_starts = HashMap::from([(parent_id.clone(), ("Codex:1".into(), 4_000))]);
    let root_rollouts = HashMap::from([(
        "01800000-0000-7000-8000-000000000001".into(),
        RootRollout {
            cwd: "/work".into(),
            started_at_ms: 1_000,
        },
    )]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &root_rollouts,
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(parent_id.as_str())
    );
}

#[test]
fn codex_thread_fallback_refuses_parent_started_after_thread() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/default/%1".into();
    parent.cwd = "/work".into();
    parent.changed_at_ms = 6_000;
    let mut records = HashMap::from([(parent.id.clone(), parent)]);
    let record_starts = HashMap::from([("host/default/%1".into(), ("Codex:1".into(), 8_000))]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 1);
    assert!(records.values().all(|record| record.subagent.is_none()));
}

#[test]
fn codex_thread_without_a_known_root_does_not_guess_by_cwd() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/default/%1".into();
    parent.cwd = "/work".into();
    parent.changed_at_ms = 6_000;
    let mut records = HashMap::from([(parent.id.clone(), parent)]);
    let record_starts = HashMap::from([("host/default/%1".into(), ("Codex:1".into(), 1_000))]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 1);
    assert!(records.values().all(|record| record.subagent.is_none()));
}

#[test]
fn codex_thread_without_a_known_root_does_not_claim_a_process_only_parent() {
    let mut parent = old(AgentState::Unknown, true);
    parent.id = "host/terminal/ttys001/10".into();
    parent.cwd = "/work".into();
    parent.origin = AgentOrigin::Terminal;
    let mut records = HashMap::from([(parent.id.clone(), parent)]);
    let record_starts = HashMap::from([(
        "host/terminal/ttys001/10".into(),
        ("Codex:10".into(), 1_000),
    )]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &HashMap::new(),
        &record_starts,
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 1);
    assert!(records.values().all(|record| record.subagent.is_none()));
}

#[test]
fn rollout_identity_enriches_process_child_without_duplication() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent.id.clone(),
        started_at_ms: 5_010,
        finished_at_ms: None,
        name: None,
        thread_id: None,
    });
    let parent_id = parent.id.clone();
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(Some(9_000))],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 2);
    let child = &records[&child_id];
    assert_eq!(child.attention, Attention::Done);
    assert_eq!(
        child
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_deref()),
        Some("01800000-0000-7000-8000-000000000002")
    );
    assert_eq!(
        child
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.name.as_deref()),
        Some("Worker")
    );
}

#[test]
fn process_backed_rollout_enriches_child_nested_under_in_process_thread() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut delegated = old(AgentState::Idle, true);
    delegated.id = "host/codex-thread/delegated".into();
    delegated.cwd = "/work".into();
    delegated.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 4_000,
        finished_at_ms: Some(6_000),
        name: Some("Banach".into()),
        thread_id: Some("delegated".into()),
    });
    let delegated_id = delegated.id.clone();
    let mut review = old(AgentState::Unknown, true);
    review.id = "host/terminal/ttys002/70".into();
    review.cwd = "/work".into();
    review.origin = AgentOrigin::Terminal;
    review.subagent = Some(SubagentInfo {
        parent_id: delegated_id,
        started_at_ms: 5_010,
        finished_at_ms: None,
        name: Some("Worker".into()),
        thread_id: None,
    });
    let review_id = review.id.clone();
    let mut records = HashMap::from([
        (parent_id.clone(), parent),
        (delegated.id.clone(), delegated),
        (review_id.clone(), review),
    ]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 3);
    assert_eq!(
        records[&review_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_deref()),
        Some("01800000-0000-7000-8000-000000000002")
    );
    assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn in_process_thread_does_not_replace_a_separate_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_010,
        finished_at_ms: None,
        name: Some("Worker".into()),
        thread_id: None,
    });
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
    let mut thread = codex_thread(None);
    thread.process_backed = false;

    link_codex_thread_subagents(
        &mut records,
        &[thread],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 3);
    assert!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_ref())
            .is_none()
    );
    assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn rollout_identity_rejects_new_process_child_with_mismatched_start_time() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent.id.clone(),
        started_at_ms: 500_000,
        finished_at_ms: None,
        name: Some("Worker".into()),
        thread_id: None,
    });
    let parent_id = parent.id.clone();
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(records.len(), 3);
    assert!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_ref())
            .is_none()
    );
    assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn rollout_identity_does_not_overwrite_process_child_bound_to_another_thread() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent.id.clone(),
        started_at_ms: 5_000,
        finished_at_ms: None,
        name: None,
        thread_id: Some("already-bound".into()),
    });
    let parent_id = parent.id.clone();
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_deref()),
        Some("already-bound")
    );
    assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn rollout_identity_does_not_enrich_a_non_codex_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.agent = "Claude".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent.id.clone(),
        started_at_ms: 5_000,
        finished_at_ms: None,
        name: None,
        thread_id: None,
    });
    let parent_id = parent.id.clone();
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(None)],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_ref())
            .is_none()
    );
    assert!(records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn exact_thread_binding_outranks_an_unbound_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut exact = old(AgentState::Unknown, true);
    exact.id = "host/terminal/ttys002/70".into();
    exact.cwd = "/work".into();
    exact.origin = AgentOrigin::Terminal;
    exact.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_000,
        finished_at_ms: None,
        name: None,
        thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
    });
    let exact_id = exact.id.clone();
    let mut unbound = exact.clone();
    unbound.id = "host/terminal/ttys003/80".into();
    unbound.subagent.as_mut().unwrap().thread_id = None;
    let unbound_id = unbound.id.clone();
    let mut records = HashMap::from([
        (parent_id.clone(), parent),
        (exact_id.clone(), exact),
        (unbound_id.clone(), unbound),
    ]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(Some(9_000))],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&exact_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.finished_at_ms),
        Some(9_000)
    );
    assert!(
        records[&unbound_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_ref())
            .is_none()
    );
    assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn exact_thread_binding_does_not_require_matching_cwd() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work-before-runner-refresh".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_000,
        finished_at_ms: None,
        name: None,
        thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
    });
    let child_id = child.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[codex_thread(Some(9_000))],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.finished_at_ms),
        Some(9_000)
    );
    assert!(!records.contains_key("host/codex-thread/01800000-0000-7000-8000-000000000002"));
}

#[test]
fn competing_rollouts_do_not_claim_one_unbound_process_child() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut child = old(AgentState::Unknown, true);
    child.id = "host/terminal/ttys002/70".into();
    child.cwd = "/work".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: parent_id.clone(),
        started_at_ms: 5_000,
        finished_at_ms: None,
        name: None,
        thread_id: None,
    });
    let child_id = child.id.clone();
    let mut first = codex_thread(None);
    first.thread_id = "first-thread".into();
    first.name = None;
    let mut second = codex_thread(None);
    second.thread_id = "second-thread".into();
    second.name = None;
    let mut records = HashMap::from([(parent_id.clone(), parent), (child_id.clone(), child)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);

    link_codex_thread_subagents(
        &mut records,
        &[second, first],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert!(
        records[&child_id]
            .subagent
            .as_ref()
            .and_then(|subagent| subagent.thread_id.as_ref())
            .is_none()
    );
    assert!(records.contains_key("host/codex-thread/first-thread"));
    assert!(records.contains_key("host/codex-thread/second-thread"));
}

#[test]
fn completed_process_thread_stays_suppressed_until_reactivation() {
    let mut child = old(AgentState::Idle, false);
    child.id = "host/terminal/ttys002/70".into();
    child.origin = AgentOrigin::Terminal;
    child.subagent = Some(SubagentInfo {
        parent_id: "host/run/main".into(),
        started_at_ms: 5_000,
        finished_at_ms: Some(10_000),
        name: Some("Worker".into()),
        thread_id: Some("01800000-0000-7000-8000-000000000002".into()),
    });
    let previous = HashMap::from([(child.id.clone(), child.clone())]);
    let mut ownership = CodexOwnership::new(&previous, "host", "default");

    let mut records = HashMap::from([(child.id.clone(), child.clone())]);
    let mut record_pids = HashMap::from([(child.id.clone(), HashSet::from([70]))]);
    ownership.suppress_finished_processes_before_linking(&mut records, &mut record_pids, &[]);
    assert!(records.is_empty());
    assert!(record_pids.is_empty());
    assert_eq!(
        ownership
            .finished_process_threads
            .get(&child.id)
            .map(String::as_str),
        Some("01800000-0000-7000-8000-000000000002")
    );

    let mut reactivated_records = HashMap::from([(child.id.clone(), child)]);
    let mut reactivated_pids =
        HashMap::from([("host/terminal/ttys002/70".into(), HashSet::from([70]))]);
    ownership.suppress_finished_processes_before_linking(
        &mut reactivated_records,
        &mut reactivated_pids,
        &[codex_thread(None)],
    );
    assert!(reactivated_records.contains_key("host/terminal/ttys002/70"));
    assert!(ownership.finished_process_threads.is_empty());
}

#[test]
fn nested_codex_thread_links_to_its_synthetic_thread_parent() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let mut records = HashMap::from([(parent_id.clone(), parent)]);
    let thread_ids = HashMap::from([(parent_id, "01800000-0000-7000-8000-000000000001".into())]);
    let mut first = codex_thread(None);
    first.thread_id = "first-thread".into();
    first.name = Some("First".into());
    let mut nested = codex_thread(None);
    nested.thread_id = "nested-thread".into();
    nested.parent_thread_id = first.thread_id.clone();
    nested.name = Some("Nested".into());
    nested.depth = Some(2);

    link_codex_thread_subagents(
        &mut records,
        &[nested, first],
        &HashMap::new(),
        &thread_ids,
        &HashMap::new(),
        &HashMap::new(),
        ("host", "default"),
    );

    assert_eq!(
        records["host/codex-thread/nested-thread"]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some("host/codex-thread/first-thread")
    );
}

#[test]
fn recovered_root_binding_is_retained_for_the_same_process_identity() {
    let mut parent = old(AgentState::Working, true);
    parent.id = "host/run/main".into();
    parent.cwd = "/work".into();
    let parent_id = parent.id.clone();
    let record_starts = HashMap::from([(parent_id.clone(), ("Codex:20".into(), 1_000))]);
    let recovered_root_threads = HashMap::from([(
        parent_id.clone(),
        "01800000-0000-7000-8000-000000000001".into(),
    )]);
    let mut ownership = CodexOwnership::new(&HashMap::new(), "host", "default");
    let mut first_records = HashMap::from([(parent_id.clone(), parent.clone())]);
    let mut first_thread_ids = HashMap::new();
    ownership.reconcile_after_process_linking(ReconciliationFrame {
        records: &mut first_records,
        record_thread_ids: &mut first_thread_ids,
        record_starts: &record_starts,
        previous: &HashMap::new(),
        threads: &[],
        root_rollouts: &HashMap::new(),
        recovered_root_threads: &recovered_root_threads,
    });

    let mut next_records = HashMap::from([(parent_id.clone(), parent)]);
    let mut next_thread_ids = HashMap::new();
    let conflicting_recovered_root_threads = HashMap::from([(
        parent_id.clone(),
        "01800000-0000-7000-8000-000000000099".into(),
    )]);
    ownership.reconcile_after_process_linking(ReconciliationFrame {
        records: &mut next_records,
        record_thread_ids: &mut next_thread_ids,
        record_starts: &record_starts,
        previous: &first_records,
        threads: &[codex_thread(None)],
        root_rollouts: &HashMap::new(),
        recovered_root_threads: &conflicting_recovered_root_threads,
    });

    assert_eq!(
        next_records["host/codex-thread/01800000-0000-7000-8000-000000000002"]
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_id.as_str()),
        Some(parent_id.as_str())
    );
}
