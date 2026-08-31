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
configuration grants federation read access only; it never grants tmux-agent
authority to run lifecycle commands on another machine.

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

When a remote session was reached through an SSH process inside local tmux,
tmux-agent matches the two endpoints of that established connection and can
focus the local pane carrying it. An ordinary remote terminal reached through
mosh can also be focused when exactly one live, unmarked local pane has a
`mosh-client` process title naming the configured remote and its normalized
pane title matches the selected agent. The process title stays local.

An integration that provides mirror panes can set:

```tmux
set -pt:. @tmux_agent_remote_host 'build-host'
set -pt:. @tmux_agent_remote_session 'agents'
```

Only the public `@tmux_agent_remote_host` and
`@tmux_agent_remote_session` marker names are recognized.

Set both markers on a local pane that attaches to a tmux session on the remote
machine. tmux-agent does not use a matching title alone for a remote tmux
agent because another SSH pane can have the same title.

Explicitly marked panes are reserved for remote tmux bindings and are not
ordinary-terminal candidates. Dead panes and tmux-agent UI panes are also
excluded. If no unique local SSH, mosh, or mirror pane can be resolved,
tmux-agent reports the ambiguity or missing focus target instead of guessing.
Ordinary mosh focus does not write pane options.

For a uniquely resolved remote agent, completion visibility requires both the
remote agent pane and its local transport pane to be visible. A completion in a
hidden local transport is shown as `done`. Opening that transport marks the
completion seen. Ambiguous and unresolved transports keep the peer's reported
visibility and attention state.

An exact host and session binding takes priority. Without one, focus can adopt
exactly one live, unmarked mosh pane that names the configured remote and is
already displaying the selected agent with the nested tmux title shape
`[mosh] · ...`. The ordinary-terminal shape `[mosh] title` is not adopted as a
tmux binding. A binding for another session on the same host does not block
this recovery. Zero or multiple matching panes are left unmarked.

For a detached default-server tmux session, focus can instead create the
initial mosh binding when exactly one unmarked pane names the configured remote
and its shell working-directory title matches the remote agent directory.
tmux-agent selects the exact remote window and pane, types the attach command
into that shell, verifies that the local pane adopts the selected agent title,
then writes the markers and focuses it. Another binding on the host does not
block a unique alias and working-directory match. Named tmux servers and zero
or multiple shell matches fail closed.

Other nested remote tmux connections, including SSH connections that cannot be
matched to the remote agent process or mosh panes without the nested title
shape, need an initial explicit local-pane binding. Run this on the local
machine before entering the remote shell, or pass the local pane ID from
another local pane:

```sh
tmux-agent remote bind build-host agents --pane %42
```

The command writes the two public marker options above. The binding belongs to
that pane and disappears with it. Inspect or remove bindings with:

```sh
tmux-agent remote bindings
tmux-agent remote unbind --pane %42
```

When `--pane` is omitted, bind and unbind use the current local `$TMUX_PANE`.
The bind command rejects names that are not present in the local tmux-agent
configuration and never chooses between several mosh or SSH panes. If the bound
remote tmux session is later recreated under a different name,
focusing its agent repairs the session marker only when that host has one live,
non-UI bound pane with a matching normalized title. Zero or multiple matches
leave the binding unchanged and report the normal focus error.

A local transport pane can also provide the label shown beside its remote
agent:

```tmux
set -pt:. @pane_label 'testing env'
```

The local label takes precedence over a remote pane label only when the
transport is resolved uniquely. An ambiguous or unrelated pane does not
contribute a label.

## Custom SSH commands

For unusual SSH setups, an explicit command vector remains available:

```toml
[[remote]]
name = "build-host"
command = ["ssh", "-T", "build-host", "tmux-agent", "watch", "--jsonl", "--local-only"]
```

The structured `[[machine]]` form is preferred because it also supports
diagnostics and remote Codex child viewing.
