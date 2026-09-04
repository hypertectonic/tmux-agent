use super::{RootRollout, ThreadRollout, normalize_name};
use crate::model::{AgentRecord, AgentState, Attention, EvidenceSource, SubagentInfo};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub(crate) struct CodexOwnership {
    host: String,
    server: String,
    finished_process_threads: HashMap<String, String>,
    process_root_threads: HashMap<String, (String, String)>,
}

pub(crate) struct ReconciliationFrame<'a> {
    pub records: &'a mut HashMap<String, AgentRecord>,
    pub record_thread_ids: &'a mut HashMap<String, String>,
    pub record_starts: &'a HashMap<String, (String, u64)>,
    pub previous: &'a HashMap<String, AgentRecord>,
    pub threads: &'a [ThreadRollout],
    pub root_rollouts: &'a HashMap<String, RootRollout>,
    pub recovered_root_threads: &'a HashMap<String, String>,
}

impl CodexOwnership {
    pub(crate) fn new(previous: &HashMap<String, AgentRecord>, host: &str, server: &str) -> Self {
        let mut ownership = Self {
            host: host.to_string(),
            server: server.to_string(),
            finished_process_threads: HashMap::new(),
            process_root_threads: HashMap::new(),
        };
        ownership.remember_finished_process_threads(previous);
        ownership
    }

    /// Removes process-backed records whose Codex rollout has already completed.
    ///
    /// Scanner calls this before its provider-neutral process-tree linker so a
    /// retained process cannot become the parent of a new generic subagent.
    pub(crate) fn suppress_finished_processes_before_linking(
        &mut self,
        records: &mut HashMap<String, AgentRecord>,
        record_pids: &mut HashMap<String, HashSet<u32>>,
        threads: &[ThreadRollout],
    ) {
        let active_thread_ids = threads
            .iter()
            .filter(|thread| thread.finished_at_ms.is_none())
            .map(|thread| thread.thread_id.as_str())
            .collect::<HashSet<_>>();
        self.finished_process_threads
            .retain(|record_id, thread_id| {
                records.contains_key(record_id) && !active_thread_ids.contains(thread_id.as_str())
            });
        for record_id in self.finished_process_threads.keys() {
            records.remove(record_id);
            record_pids.remove(record_id);
        }
    }

    /// Reconciles Codex ownership after Scanner has linked process-backed
    /// subagents and restored provider-neutral ancestry.
    pub(crate) fn reconcile_after_process_linking(&mut self, frame: ReconciliationFrame<'_>) {
        let ReconciliationFrame {
            records,
            record_thread_ids,
            record_starts,
            previous,
            threads,
            root_rollouts,
            recovered_root_threads,
        } = frame;

        self.process_root_threads
            .retain(|record_id, (identity, _)| {
                records.get(record_id).is_some_and(|record| {
                    record.agent.eq_ignore_ascii_case("codex") && record.subagent.is_none()
                }) && record_starts
                    .get(record_id)
                    .is_some_and(|(current, _)| current == identity)
            });
        for (record_id, (_, thread_id)) in &self.process_root_threads {
            record_thread_ids
                .entry(record_id.clone())
                .or_insert_with(|| thread_id.clone());
        }
        for (record_id, thread_id) in recovered_root_threads {
            if record_thread_ids.contains_key(record_id) {
                continue;
            }
            record_thread_ids.insert(record_id.clone(), thread_id.clone());
            if let Some((identity, _)) = record_starts.get(record_id) {
                self.process_root_threads
                    .insert(record_id.clone(), (identity.clone(), thread_id.clone()));
            }
        }

        link_thread_subagents(
            records,
            threads,
            root_rollouts,
            record_thread_ids,
            record_starts,
            previous,
            (&self.host, &self.server),
        );
        self.remember_finished_process_threads(records);
    }

    fn remember_finished_process_threads(&mut self, records: &HashMap<String, AgentRecord>) {
        let synthetic_prefix = format!("{}/codex-thread/", self.host);
        for record in records.values() {
            let Some(subagent) = record
                .subagent
                .as_ref()
                .filter(|subagent| subagent.finished_at_ms.is_some())
            else {
                continue;
            };
            if !record.id.starts_with(&synthetic_prefix)
                && let Some(thread_id) = &subagent.thread_id
            {
                self.finished_process_threads
                    .insert(record.id.clone(), thread_id.clone());
            }
        }
    }
}

fn link_thread_subagents(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    root_rollouts: &HashMap<String, RootRollout>,
    record_thread_ids: &HashMap<String, String>,
    record_starts: &HashMap<String, (String, u64)>,
    previous: &HashMap<String, AgentRecord>,
    location: (&str, &str),
) {
    let (host, server) = location;
    let explicit_parents = unambiguous_thread_parents(record_thread_ids);
    let thread_ids = threads
        .iter()
        .map(|thread| thread.thread_id.as_str())
        .collect::<HashSet<_>>();
    let mut linked_threads = records
        .values()
        .filter_map(|record| {
            record
                .subagent
                .as_ref()
                .and_then(|subagent| subagent.thread_id.as_ref())
                .map(|thread_id| (thread_id.clone(), record.id.clone()))
        })
        .collect::<HashMap<_, _>>();
    let mut pending = threads.iter().collect::<Vec<_>>();
    pending.sort_by_key(|thread| (thread.depth.unwrap_or(1), thread.started_at_ms));
    loop {
        let mut progress = false;
        let mut remaining = Vec::new();
        for thread in pending {
            let parent_id = explicit_parents
                .get(&thread.parent_thread_id)
                .or_else(|| linked_threads.get(&thread.parent_thread_id))
                .cloned()
                .or_else(|| {
                    (thread.depth.unwrap_or(1) <= 1
                        && !thread_ids.contains(thread.parent_thread_id.as_str()))
                    .then(|| {
                        root_rollouts
                            .get(&thread.parent_thread_id)
                            .and_then(|root| unique_root_parent(records, root, record_starts))
                    })
                    .flatten()
                });
            let Some(parent_id) = parent_id else {
                remaining.push(thread);
                continue;
            };
            if let Some(record_id) =
                attach_thread_to_process_child(records, threads, thread, &parent_id, previous)
            {
                reparent_agent_path_process_child(records, threads, thread, &parent_id, &record_id);
                linked_threads.insert(thread.thread_id.clone(), record_id);
                progress = true;
                continue;
            }
            let synthetic_id = format!("{host}/codex-thread/{}", thread.thread_id);
            if insert_synthetic_thread(records, thread, &parent_id, &synthetic_id, server) {
                reparent_agent_path_process_child(
                    records,
                    threads,
                    thread,
                    &parent_id,
                    &synthetic_id,
                );
                linked_threads.insert(thread.thread_id.clone(), synthetic_id);
                progress = true;
            } else {
                remaining.push(thread);
            }
        }
        if !progress || remaining.is_empty() {
            break;
        }
        pending = remaining;
    }
}

fn reparent_agent_path_process_child(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    root_parent_id: &str,
    thread_record_id: &str,
) {
    if thread.process_backed {
        return;
    }
    let mut matches = records.values().filter(|record| {
        record.id != thread_record_id
            && record.agent.eq_ignore_ascii_case("codex")
            && same_cwd(&record.cwd, &thread.cwd)
            && record.subagent.as_ref().is_some_and(|subagent| {
                subagent.parent_id == root_parent_id
                    && subagent.thread_id.is_none()
                    && subagent
                        .name
                        .as_deref()
                        .map(normalize_name)
                        .is_some_and(|name| {
                            preferred_agent_path_owner(
                                threads,
                                thread,
                                &name,
                                subagent.started_at_ms,
                            )
                        })
            })
    });
    let Some(child_id) = matches.next().map(|record| record.id.clone()) else {
        return;
    };
    if matches.next().is_some() {
        return;
    }
    if let Some(subagent) = records
        .get_mut(&child_id)
        .and_then(|record| record.subagent.as_mut())
    {
        subagent.parent_id = thread_record_id.to_string();
    }
}

fn preferred_agent_path_owner(
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    process_name: &str,
    process_started_at_ms: u64,
) -> bool {
    let mut candidates = threads
        .iter()
        .filter(|candidate| {
            !candidate.process_backed
                && candidate.parent_thread_id == thread.parent_thread_id
                && same_cwd(&candidate.cwd, &thread.cwd)
                && candidate.started_at_ms <= process_started_at_ms
                && candidate
                    .finished_at_ms
                    .is_none_or(|finished_at_ms| process_started_at_ms <= finished_at_ms)
        })
        .filter_map(|candidate| {
            agent_path_match_score(candidate, process_name)
                .map(|score| (candidate, (candidate.started_at_ms, score)))
        });
    let Some((best, score)) = candidates.next() else {
        return false;
    };
    let mut best_thread_id = best.thread_id.as_str();
    let mut best_score = score;
    let mut tied = false;
    for (candidate, score) in candidates {
        if score > best_score {
            best_thread_id = candidate.thread_id.as_str();
            best_score = score;
            tied = false;
        } else if score == best_score {
            tied = true;
        }
    }
    !tied && best_thread_id == thread.thread_id
}

fn agent_path_match_score(thread: &ThreadRollout, process_name: &str) -> Option<u8> {
    let path_name = thread
        .agent_path
        .as_deref()?
        .rsplit('/')
        .find(|component| !component.is_empty())
        .map(normalize_name)?;
    let expected_name = path_name.strip_prefix("codex-").unwrap_or(&path_name);
    if expected_name == process_name {
        Some(1)
    } else {
        expected_name
            .strip_suffix(process_name)
            .filter(|prefix| prefix.ends_with('-'))
            .map(|_| 0)
    }
}

fn unique_root_parent(
    records: &HashMap<String, AgentRecord>,
    root: &RootRollout,
    record_starts: &HashMap<String, (String, u64)>,
) -> Option<String> {
    const ROOT_START_TOLERANCE_MS: u64 = 10_000;

    let mut matches = records
        .values()
        .filter(|record| {
            record.agent.eq_ignore_ascii_case("codex")
                && record.subagent.is_none()
                && matches!(
                    record.state,
                    AgentState::Working
                        | AgentState::Blocked
                        | AgentState::Idle
                        | AgentState::Unknown
                )
                && same_cwd(&record.cwd, &root.cwd)
                && record_starts.get(&record.id).is_some_and(|(_, started)| {
                    started.abs_diff(root.started_at_ms) <= ROOT_START_TOLERANCE_MS
                })
        })
        .map(|record| record.id.clone());
    let parent = matches.next()?;
    matches.next().is_none().then_some(parent)
}

fn insert_synthetic_thread(
    records: &mut HashMap<String, AgentRecord>,
    thread: &ThreadRollout,
    parent_id: &str,
    synthetic_id: &str,
    server: &str,
) -> bool {
    let Some(parent) = records.get(parent_id).cloned() else {
        return false;
    };
    let finished = thread.finished_at_ms;
    let state = if finished.is_some() {
        AgentState::Idle
    } else {
        AgentState::Working
    };
    let attention = if finished.is_some() {
        Attention::Done
    } else {
        Attention::Working
    };
    let name = thread.name.clone();
    records.insert(
        synthetic_id.to_string(),
        AgentRecord {
            id: synthetic_id.to_string(),
            host: parent.host,
            server: server.to_string(),
            pane_id: parent.pane_id,
            pane_pid: parent.pane_pid,
            session_id: parent.session_id,
            session_name: parent.session_name,
            window_id: parent.window_id,
            window_index: parent.window_index,
            window_name: parent.window_name,
            pane_index: parent.pane_index,
            agent: "Codex".into(),
            state,
            attention,
            source: EvidenceSource::Process,
            title: name.clone().unwrap_or_else(|| "subagent".into()),
            label: None,
            cwd: thread.cwd.clone(),
            visible: false,
            seen: false,
            changed_at_ms: finished.unwrap_or(thread.started_at_ms),
            origin: parent.origin,
            terminal: parent.terminal,
            remote_alias: None,
            ssh_connection: parent.ssh_connection,
            session_connections: parent.session_connections,
            focus_target: None,
            goal: None,
            subagent: Some(SubagentInfo {
                parent_id: parent_id.to_string(),
                started_at_ms: thread.started_at_ms,
                finished_at_ms: finished,
                name,
                thread_id: Some(thread.thread_id.clone()),
            }),
            detection: None,
        },
    );
    true
}

fn unambiguous_thread_parents(
    record_thread_ids: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut candidates = HashMap::<String, Vec<String>>::new();
    for (record_id, thread_id) in record_thread_ids {
        candidates
            .entry(thread_id.clone())
            .or_default()
            .push(record_id.clone());
    }
    candidates
        .into_iter()
        .filter_map(|(thread_id, records)| {
            (records.len() == 1).then(|| (thread_id, records[0].clone()))
        })
        .collect()
}

fn same_cwd(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn attach_thread_to_process_child(
    records: &mut HashMap<String, AgentRecord>,
    threads: &[ThreadRollout],
    thread: &ThreadRollout,
    parent_id: &str,
    previous: &HashMap<String, AgentRecord>,
) -> Option<String> {
    let base_candidate = |record: &AgentRecord| {
        record.agent.eq_ignore_ascii_case("codex")
            && record
                .subagent
                .as_ref()
                .is_some_and(|subagent| subagent.parent_id == parent_id)
    };
    let mut exact = records
        .iter()
        .filter(|(_, record)| {
            record.agent.eq_ignore_ascii_case("codex")
                && is_subagent_descendant_of(records, record, parent_id)
                && record
                    .subagent
                    .as_ref()
                    .and_then(|subagent| subagent.thread_id.as_deref())
                    == Some(thread.thread_id.as_str())
        })
        .map(|(id, _)| id.clone());
    if let Some(candidate) = exact.next() {
        if exact.next().is_none() {
            return update_process_child(records, thread, candidate);
        }
        return None;
    }
    let mut candidates = records
        .iter()
        .filter(|(record_id, record)| {
            if !base_candidate(record) || !same_cwd(&record.cwd, &thread.cwd) {
                return false;
            }
            heuristic_process_match(record_id, record, thread, parent_id, previous)
                && threads
                    .iter()
                    .filter(|candidate_thread| {
                        candidate_thread.parent_thread_id == thread.parent_thread_id
                            && heuristic_process_match(
                                record_id,
                                record,
                                candidate_thread,
                                parent_id,
                                previous,
                            )
                    })
                    .take(2)
                    .count()
                    == 1
        })
        .map(|(id, _)| id.clone());
    if let Some(candidate) = candidates.next() {
        if candidates.next().is_some() {
            return None;
        }
        return update_process_child(records, thread, candidate);
    }

    let mut nested_candidates = records
        .iter()
        .filter(|(record_id, record)| {
            heuristic_nested_process_match(records, record_id, record, thread, parent_id)
                && threads
                    .iter()
                    .filter(|candidate_thread| {
                        candidate_thread.parent_thread_id == thread.parent_thread_id
                            && heuristic_nested_process_match(
                                records,
                                record_id,
                                record,
                                candidate_thread,
                                parent_id,
                            )
                    })
                    .take(2)
                    .count()
                    == 1
        })
        .map(|(id, _)| id.clone());
    let candidate = nested_candidates.next()?;
    if nested_candidates.next().is_some() {
        return None;
    }
    update_process_child(records, thread, candidate)
}

fn heuristic_process_match(
    _record_id: &str,
    record: &AgentRecord,
    thread: &ThreadRollout,
    parent_id: &str,
    _previous: &HashMap<String, AgentRecord>,
) -> bool {
    if !thread.process_backed {
        return false;
    }
    let Some(subagent) = &record.subagent else {
        return false;
    };
    if !record.agent.eq_ignore_ascii_case("codex")
        || subagent.parent_id != parent_id
        || !same_cwd(&record.cwd, &thread.cwd)
        || subagent.thread_id.is_some()
    {
        return false;
    }
    let expected_name = thread.name.as_deref().map(normalize_name);
    let candidate_name = subagent.name.as_deref().map(normalize_name);
    (subagent.started_at_ms.abs_diff(thread.started_at_ms) <= 120_000)
        && (expected_name.is_none() || candidate_name.is_none() || expected_name == candidate_name)
}

fn heuristic_nested_process_match(
    records: &HashMap<String, AgentRecord>,
    _record_id: &str,
    record: &AgentRecord,
    thread: &ThreadRollout,
    parent_id: &str,
) -> bool {
    if !thread.process_backed || !is_subagent_descendant_of(records, record, parent_id) {
        return false;
    }
    let Some(subagent) = &record.subagent else {
        return false;
    };
    if subagent.parent_id == parent_id
        || !record.agent.eq_ignore_ascii_case("codex")
        || !same_cwd(&record.cwd, &thread.cwd)
        || subagent.thread_id.is_some()
    {
        return false;
    }
    let expected_name = thread.name.as_deref().map(normalize_name);
    let candidate_name = subagent.name.as_deref().map(normalize_name);
    (subagent.started_at_ms.abs_diff(thread.started_at_ms) <= 120_000)
        && (expected_name.is_none() || candidate_name.is_none() || expected_name == candidate_name)
}

fn is_subagent_descendant_of(
    records: &HashMap<String, AgentRecord>,
    record: &AgentRecord,
    ancestor_id: &str,
) -> bool {
    let Some(mut parent_id) = record
        .subagent
        .as_ref()
        .map(|subagent| subagent.parent_id.as_str())
    else {
        return false;
    };
    let mut visited = HashSet::new();
    loop {
        if parent_id == ancestor_id {
            return true;
        }
        if !visited.insert(parent_id.to_string()) {
            return false;
        }
        let Some(parent) = records.get(parent_id) else {
            return false;
        };
        let Some(subagent) = &parent.subagent else {
            return false;
        };
        parent_id = &subagent.parent_id;
    }
}

fn update_process_child(
    records: &mut HashMap<String, AgentRecord>,
    thread: &ThreadRollout,
    candidate: String,
) -> Option<String> {
    let record = records.get_mut(&candidate)?;
    let subagent = record.subagent.as_mut()?;
    subagent.thread_id = Some(thread.thread_id.clone());
    subagent.started_at_ms = thread.started_at_ms;
    if subagent.name.is_none() {
        subagent.name = thread.name.clone();
    }
    if let Some(finished_at_ms) = thread.finished_at_ms {
        subagent.finished_at_ms = Some(finished_at_ms);
        record.state = AgentState::Idle;
        record.attention = Attention::Done;
        record.seen = false;
        record.changed_at_ms = finished_at_ms;
    }
    Some(candidate)
}

#[cfg(test)]
mod tests;
