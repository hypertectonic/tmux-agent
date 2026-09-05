# Remote machines

tmux-agent federates selected machines over SSH. It does not expose an
application TCP port and does not define a separate remote trust model.

## Requirements

- The same tmux-agent application and protocol version on every machine.
- Working non-interactive SSH authentication from the central machine.
- Accepted SSH host keys.
- An absolute path to the remote tmux-agent binary.

Tailscale, a private LAN, a jump host, or another route can make SSH reachable.
tmux-agent only invokes SSH and leaves network policy to the user.

## Configuration

Add a structured machine to `~/.config/tmux-agent/config.toml`:

```toml
[[machine]]
name = "build-host"
host = "build-host.example.ts.net"
ssh_user = "agent"
binary = "/home/agent/.local/bin/tmux-agent"
```

Set `auto_connect = false` when the machine should remain configured for
interactive operations but should not start a background collector.

## Setup order

1. From a user-controlled shell, update each remote explicitly with ordinary
   SSH. tmux-agent does not issue this command or orchestrate remote updates:

   ```sh
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent update'
   ```
2. Inspect the remote managed versions, then verify the selected binary:

   ```sh
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent versions'
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent --version'
   ```

3. Update the central machine.
4. Restart the central daemon:

   ```sh
   tmux-agent daemon restart
   ```

5. Verify peers:

   ```sh
   tmux-agent doctor
   tmux-agent list
   ```

Federation protocol changes require all machines to be updated together.
Protocol mismatches are rejected with both the received and required version.

If a remote update needs to be reversed, choose a version from that host's
`versions` output and explicitly run:

```sh
ssh -T agent@build-host.example.ts.net \
  '/home/agent/.local/bin/tmux-agent rollback <version>'
```

These are ordinary SSH commands initiated and authorized by the user. Remote
configuration permits federation reads and explicit focus or child-view actions;
it never grants tmux-agent authority to run lifecycle commands on another machine.

## What crosses SSH

Normal federation snapshots contain derived metadata such as provider, state,
attention, title, location, numeric lifecycle timestamps, goal state without
its objective, and process relationships reduced to identifiers.
Local last-used focus timestamps are not part of federation snapshots.

Snapshots do not contain:

- captured pane contents or terminal screen buffers
- prompts, responses, or reasoning
- raw process command lines
- goal objectives
- rollout events or child transcripts

Opening a remote Codex child starts a separate interactive SSH command for the
read-only viewer. That explicit action can display assistant messages and tool
output. The content does not enter the normal federation snapshot or central
persistence.

## Remote focus

For remote tmux, tmux-agent finds the session currently attached through each
local SSH or Mosh pane. Selecting an agent in a hidden remote window focuses
its outer transport and selects the requested inner window and pane, even when
the active remote window contains only a shell or editor. Multiple sessions on
one host resolve independently. Two local
transports attached to the same selected session are ambiguous.

This discovery needs `lsof` on both machines. The remote collector reads the
owning tmux server's live clients and follows each client's ancestry to its SSH
or Mosh server socket. Local Mosh clients expose their numeric server endpoint
after the bootstrap SSH connection ends, so discovery survives Mosh roaming.
Named tmux servers are supported when the configured remote collector targets
that server. No launch wrapper, registry, extra service, or dotfile edit is
needed.

Linux SSH login processes can hide their sockets from the logged-in user. In
that case tmux-agent reads only `SSH_CONNECTION` from the attached tmux client's
current login ancestry and checks it against an established inbound connection.
It does not need root, changed process permissions, or session-global tmux
environment. Missing or conflicting evidence leaves discovery incomplete;
multiple terminal channels sharing one SSH connection remain ambiguous.

Custom Mosh clients are recognized when the original `--client` option names
the exact running executable and its process title carries a numeric server
endpoint. Custom executable paths containing whitespace are not supported.

Session switches update the association on the next scan. Disconnect removes
it; reconnect discovers a new client. Socket and process updates can take about
one second. Dead panes and tmux-agent UI panes are excluded. Stale host/session
markers cannot override a known live association, and no arbitrary same-host
pane is selected when the selected session has no matching transport.

Inner selection uses a separate non-interactive SSH command from the configured
`[[machine]]`, including when the visible transport uses Mosh and bootstrap SSH
has exited. Both peers must advertise `remote_tmux_focus_v1`. The operation sends
typed JSON on stdin, validates the configured tmux server's PID and start time,
session ID and creation time, live client endpoint, and requested window and pane
IDs, then confirms the selection. Names with spaces are supported. No shell text
is sent to an interactive pane and no extra terminal opens.

The remote binary must use the same configuration for `watch` and `remote-focus`.
For a named server, set `tmux_args` in that remote configuration or use the same
configured binary wrapper for both commands. If SSH control reaches a different
server, the operation rejects the request rather than searching other servers.
The complete SSH operation has a five-second timeout and bounded input/output.

Tmux shares window selection among clients attached to the same session, and
pane selection among views of the same window, including linked windows.
Selecting an agent therefore changes what those clients see. It does not switch
any client to another session. A second local transport to the same session
remains ambiguous and prevents selection.

On the local machine, activation selects the current tmux client once before
contacting SSH control. If the transport is in another local session, only
that client switches sessions; spectators remain in the original UI session.
A target in the same session retains tmux's shared window selection, and pane
selection affects every view of that window, so those views may change together.
Remote confirmation verifies the initiating client's selected session, window,
and pane; it does not switch a second client or undo navigation while control
was running. Detaching or changing selection during control reports unconfirmed
focus.

Older peers without the capability, raw `[[remote]]` collectors, and explicit
bindings without inspectable client evidence retain outer-only focus. The UI
reports why inner selection is unavailable and keeps a popup open; the CLI prints
the notice to stderr and exits successfully. Activating a completion or pending
goal achievement still acknowledges it, but partial focus does not change
last-used ordering. A missing outer target still permits the UI's existing
acknowledgement action; ambiguous transports remain errors.

If the target disappears, the association changes, or SSH control fails, the
operation reports that the outer pane was focused but inner focus was not
confirmed. These failures keep the popup open and do not acknowledge or update
last-used ordering. Only a confirmed remote selection followed by a rechecked
local transport and initiating client returns exact focus and allows the popup
to close.

Completion visibility requires both the remote agent pane and its uniquely
resolved local transport to be visible. Outer-only focus leaves hidden remote
agents hidden; successful inner selection becomes visible on the next remote
snapshot. A transport label takes precedence over a remote pane
label only for a unique association:

```tmux
set -pt:. @pane_label 'testing env'
```

If client ancestry or sockets cannot be inspected, explicitly bind the local
pane to the selected remote session:

```sh
tmux-agent remote bind build-host agents --pane %42
tmux-agent remote bindings
tmux-agent remote unbind --pane %42
```

The binding uses pane-local `@tmux_agent_remote_host` and
`@tmux_agent_remote_session` options and disappears with that pane. Omitting
`--pane` uses the current local `$TMUX_PANE`. Bind validates the configured
remote name and never chooses a pane for you. Update the binding yourself after
switching sessions when automatic inspection is unavailable.

Automatic association may be unavailable for translated server addresses,
SSH proxy or multiplex setups that obscure the connection, or remote tmux
clients running inside another tmux server. When every attached client's
transport is inspectable, no local match fails closed even when old markers
exist. Restore direct endpoint visibility for these cases. If some attached
clients cannot be inspected, an exact explicit binding can identify an
uninspectable client even when other known clients have no local match. You
must update that binding after switching sessions.

Ordinary remote terminals retain their existing SSH connection matching and
unique unmarked Mosh destination/title matching. Older compatibility records
without attachment metadata retain legacy binding/title recovery; live peers
must all use federation protocol 4.

## Custom SSH commands

For unusual SSH setups, an explicit command vector remains available:

```toml
[[remote]]
name = "build-host"
command = ["ssh", "-T", "build-host", "tmux-agent", "watch", "--jsonl", "--local-only"]
```

The structured `[[machine]]` form is preferred because it also supports
diagnostics, inner tmux focus, and remote Codex child viewing. Raw collector
commands do not define a focus control channel; tmux-agent does not infer one
from their command text.
