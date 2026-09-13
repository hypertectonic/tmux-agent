---
name: tmux-agent-workspace
description: Create a new interactive coding-agent session in tmux, follow through on its assigned task, and clean up its workers when requested. Use for requests such as start a fresh Codex session or launch another agent instance, and for explicit task-window or session creation. Native harness subagent requests stay with that harness's subagent mechanism.
---

# Agent workspace

Use native tmux for placement and the existing tmux-agent CLI for discovery and
handoff. Any initiating harness with shell access can follow this workflow.
There is no required dotfiles setup, provider SDK, or background supervisor.
This skill creates no new tmux-agent commands.

## Interpret the request

- "Spawn subagents" means the initiating harness's native subagent mechanism,
  not this workflow. Do not silently substitute independent tmux sessions when
  that mechanism is unavailable.
- "Start a fresh Codex session/instance to handle this" means a new interactive
  TUI, not a native subagent or a headless `codex exec` run. Start a new
  conversation, not a resume/fork or a new installation/home directory.
- Default the worker command to `codex` as resolved on the target machine.
  Honor an explicitly requested provider, command such as `codex2`, executable
  path, or `CODEX_HOME`. Do not search for alternate installations, copy the
  caller's home override, or silently substitute commands. Preserve applicable
  launcher and permission instructions; do not add permission-bypass flags.
- A request only to open a session or create layout does not imply a task or
  ongoing monitoring. Discussion and handoff drafts do not authorize spawning.

## Resolve placement and checkout

Default to a new window in the caller agent's actual tmux session, on the
machine where that agent runs. Derive the server and pane from the caller's
execution context, normally `TMUX` and `TMUX_PANE`, and verify that they belong
to that agent. An executor may have missing or inherited context; never replace
it with whichever pane or client the human currently has selected.

For an agent inside remote/nested tmux, operate on its inner server on that
machine. Do not create a window in an outer SSH/Mosh viewing pane. If outside
tmux or unable to verify the caller, ask for the destination machine and
session/server. Another machine/session, a new session, or an existing-window
split requires that placement to be requested. Remote creation uses authorized
shell access on that host; tmux-agent federation is not a window-creation API.

Keep the verified machine, socket path, server PID, session ID, window ID and
pane ID. Use explicit socket and ID targets for later operations; names and
window/pane indices are for readable reports. Revalidate after moves or restarts
instead of looking up a replacement by name alone.

Use the requested checkout and existing repository Git/worktree instructions.
For editing tasks, avoid overlapping writers through the existing worktree
workflow; this skill neither selects a worktree manager nor creates a worktree
for every worker. If the checkout is not established, resolve it before launch.

## Create layout, then launch

Use a short task-based name such as `review-parser`. If occupied, choose an
unused suffix such as `review-parser-2`; never reuse or replace an existing
window merely because its name matches. Capture native returned IDs, including
when concurrent creation or user hooks change the visible layout.
If creation reports an error, inspect the server before retrying: a failing
hook can leave the new window or session created despite the error.

With `server_socket`, `caller_session_id`, `window_name`, and `checkout` already
verified on the destination machine, the normal window-creation operation is:

```sh
tmux -S "$server_socket" new-window -d -P \
  -F '#{pid} #{session_id} #{window_id} #{pane_id}' \
  -t "${caller_session_id}:" -n "$window_name" -c "$checkout"
```

For an explicitly requested new session on that server:

```sh
tmux -S "$server_socket" new-session -d -P \
  -F '#{pid} #{session_id} #{window_id} #{pane_id}' \
  -s "$session_name" -c "$checkout"
```

Do not use replace/kill or attach-if-existing options. Preserve current focus
by creating detached, and leave existing windows/layouts alone. Let ordinary
tmux creation hooks run. Do not copy a layout or recreate its running commands,
force a sidebar, change global options, or disable the user's hooks. If hooks
themselves change focus or occupy the worker pane, report that rather than
undoing user configuration or killing its processes.

The initial pane returned by creation is the default worker target even if a
hook adds other panes. Inspect it after hooks finish. Launch only into that
verified, newly created idle shell with an empty prompt, in the intended
checkout. If it was replaced or is already running something, stop and resolve
the target. Do not kill or repurpose it automatically.

Give the verified worker pane a short task label before launch, such as
`review: handoff spec` or `research: nested tmux`. Read its existing `@pane_label`
first and preserve a non-empty custom label unless the user requested changing
it. When unset, set the pane-local option using the captured pane ID:

```sh
tmux -S "$server_socket" set-option -p -t "$worker_pane_id" @pane_label "$task_label"
```

Leave the provider-controlled terminal title alone. Label only the actual
worker pane, including on the inner server for remote/nested tmux, not its
outer SSH/Mosh viewer or sibling panes. Keep labels concise and free of secrets;
they are discoverable metadata, not private notes or proof of ownership.

Enter the launcher command literally in that shell and submit once. This
initial shell command is separate from task delivery. Shell-quote paths and
arguments, keep credentials out of the command, and use the appropriate
interactive shell when the requested launcher is an alias or function. If the
requested command cannot resolve there, report the failure without fallback.
Leave the full provider TUI available for the user to enter and take over.

## Discover and deliver

For an assigned task, check `tmux-agent find --help` and `tmux-agent handoff
--help` before creating a worker. Use a tmux-agent config that discovers the
intended server; do not silently install builds or change peer configuration.

Poll discovery briefly with a bounded startup wait, for example up to one
minute, then report a startup blocker instead of creating duplicate workers.
Match the new pane's exact endpoint to its full discovered agent ID, provider
and checkout. Inspect the new pane's output for the normal input prompt: an
`idle` record alone does not prove readiness past login or trust dialogs. Do not
accept those dialogs or cancel tmux copy mode automatically.

Check the discovered `label` in `tmux-agent find --json` or `list --json`.
The current CLI has no `--label` filter, and `--title` does not search labels;
inspect/filter the JSON, then use the full agent ID for delivery. A matched
outer transport pane's label can override the inner label in a remote snapshot.
Do not overwrite that viewer's label or mistake a label match for exact identity.

Read the companion [handoff skill](../tmux-agent-handoff/SKILL.md) before sending;
if installed separately, locate its installed instructions or read the matching
repository copy. If unavailable, use the documented CLI help and delivery
rules in [Agent handoff](../../docs/handoff.md). Remote delivery requires the
configured recipient and `handoff_v1` capability. No raw tmux prompt-injection
fallback for task delivery.

Send a self-contained assignment: task, checkout/branch/reference, constraints,
expected result, and what evidence to report. A fresh worker does not inherit
the caller's conversation. Use the exact discovered ID and retain the handoff
ID. Distinguish window creation, successful launch, confirmed submission, and
observed task acceptance. An uncertain send is not permission to resend under
a new ID or create another worker.

## Follow through on assigned tasks

The initiating agent monitors until the assigned outcome is verified, a blocker
requires user input, or the user changes the instruction. Check tmux-agent state
at a modest interval, for example every 30 seconds, rather than busy polling.
Inspect the exact worker pane or its relevant transcript when needed to see
progress, questions and results. Remote output inspection requires authorized
access on the owning machine; federation snapshots do not contain transcripts.
Keep the user informed without repeatedly reading the entire conversation.

Respond to in-scope questions through handoff. Do not answer permission prompts,
expand scope, or automatically restart a stalled worker. A `done`/`idle` state
or a delivered handoff is not proof of completion. Read the worker's final
report and check the requested evidence before reporting success. If the worker
disappears or progress cannot be established, report the uncertainty.

Report the machine, session name and window.pane location, checkout, handoff ID,
result and remaining blockers. Retain exact IDs for further operations.
Monitoring lasts only while the initiating agent remains active. Do not imply
automatic supervision or completion notifications after it stops.

## Clean up workers when requested

Leave finished workers open for inspection by default. Keep the task's created
endpoints, checkout and handoff IDs in the initiating conversation so a later
"clean up workers from this task" has an exact scope. Do not create a separate
ledger or per-worker files. Missing ownership information is a reason to ask,
not to infer ownership from a window name or an idle Codex process.

Cleanup can be authorized upfront: "Start two fresh Codex sessions, monitor
them, collect their results, then clean up their windows." In that case, collect
and verify the results before cleanup. Otherwise wait for an explicit cleanup
request; never close workers on an idle timer or solely because they show done.

Before closing anything, report the intended targets and inspect their current
machine/server/window/pane identities, processes and output. Confirm the assigned
work finished, its result is retained, and there are no running jobs, unanswered
questions or unsaved work that closing would lose. Skip repurposed panes,
replacement processes and anything now used for another task. If identity or
completion is uncertain, leave it open and explain why.

Exit eligible workers gracefully using the provider's supported exit mechanism,
then verify they stopped. Do not escalate to force-killing an unresponsive
worker without further direction. Remove only the task's eligible panes. Remove
a whole window only when every remaining pane is within the authorized cleanup
scope; preserve added panes, hook-created services and unrelated work. Window
cleanup does not authorize killing the containing session or tmux server. If
removing the last pane/window would also end that session/server, ask first.

Worktree and Git cleanup remain separate. Closing a worker does not authorize
deleting its checkout, branch, transcript or unmerged changes, returning its
worktree lease, or committing/stashing work to make cleanup possible. Report
which workers/windows were closed, which were kept and why, and the checkouts
left available.
