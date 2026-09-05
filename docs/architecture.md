# Architecture

tmux-agent is local-first. One daemon per tmux server collects local state,
serves a user-only Unix socket, and optionally aggregates explicitly configured
SSH peers.

```text
provider process -> local scanner -> local daemon -> terminal UI
owned PTY runner --------^              ^
                                        |
remote daemon watch -> SSH collector ---+
```

## Local discovery

The scanner combines tmux pane metadata, foreground process groups, ordinary
TTY process groups, and owned PTY records. Captured terminal surfaces are used
only on the machine that owns them.

The daemon refreshes the global process table and process-derived socket
inventory at most once per second. Every scan still reads current tmux pane
metadata and projects those panes against the latest process inventory.
Candidate panes in a window displayed by any attached tmux client are captured
on every normal scan. Candidate panes in hidden windows reuse their last
successful screen and are captured on the first scan at least one second after
the previous attempt, normally about 1.0 to 1.3 seconds with the default 300 ms
scan interval. Newly eligible panes and panes whose foreground process identity
changes are captured immediately. A failed hidden capture discards the cached
screen and waits for the same background interval before retrying. Replaying a
cached quiet screen does not count as a new idle observation, while current
tmux metadata and title evidence are still evaluated on every scan.

Process starts, exits, and SSH connection changes can therefore take about one
second to appear. Tmux metadata and screen-derived state in displayed windows
remain on the normal scan cadence.

Codex, Claude, OpenCode, Grok, OMP, and Pi each have a typed detector backed by
minimal synthetic fixtures. Detectors emit semantic state and derived evidence
details, not the source terminal content. State stabilization prevents one
quiet frame from immediately erasing stronger working or blocked evidence.

OMP v17.3.4 enables state titles by default. Its exact `π <separator>
<label>` title is the primary state signal. The scanner stores only `<label>`,
so the 80 ms working spinner does not change the record title or publish a new
snapshot. If OMP disables or overrides state titles, its detector falls back
to narrow visible status and permission markers.

The owned PTY runner transparently forwards input, output, resize events,
signals, job control, and child exit status. Its mode `0600` heartbeat record
contains only process identity, terminal identity, working directory, derived
state, and detector diagnostics. Records expire after three missed heartbeats.

## State and attention

Semantic states are `working`, `blocked`, `idle`, and `unknown`. Attention is
ordered as:

```text
blocked > done > working > idle > unknown
```

`done` is derived when an active agent becomes idle while its tmux window is
not visible. Activating the row or using `acknowledge` marks the completion
seen. Codex goal achievements use the same explicit acknowledgement boundary.
Within the idle bucket, top-level agents sort by the newer of their state-change
time and their last successful focus through tmux-agent. The daemon keeps focus
times in memory for its own tmux server and discards them when agents disappear
or the daemon restarts. Other attention buckets and subagent ordering keep their
existing location and lifecycle ordering.

## Subagents

Process-backed agents are linked to the nearest discovered ancestor in the
process tree. Codex in-process children use bounded metadata from local rollout
files. Only thread identity, parent identity, nickname, working directory, and
lifecycle timestamps participate in discovery. Rollout content never enters a
snapshot.

The scanner owns provider-neutral process-tree linking, ancestry restoration,
and the 30-second finished-child retention window. Rollout discovery and
metadata parsing stay on the Codex side of the scanner boundary, with open-file
inspection isolated in the Codex evidence adapter. A focused, deterministic
Codex ownership reconciler then applies exact thread identity, recovered root
bindings, process/in-process deduplication, nesting, and fail-closed ambiguity
rules. It owns the short-lived Codex binding state but performs no tmux,
process-table, `lsof`, or filesystem I/O.

Nested parent identity is retained while visible children are active. Finished
children remain visible for 30 seconds.

## Local IPC and protocol

The daemon listens on a mode `0600` Unix socket. Requests and responses are one
JSON object per line:

- `snapshot` returns the current state
- `watch` sends the initial snapshot and later revisions
- `acknowledge` marks a completion seen
- `mark_all_read` marks every unread completion in the combined snapshot seen
- `mark_used` records a successful top-level focus for local idle ordering
- `shutdown` stops the daemon cleanly

The current federation protocol version is `4`. Live federation requires equal
protocol versions on all machines. `mark_used` is local IPC metadata: its
timestamps are neither serialized nor included in `--local-only` federation
snapshots, so it does not change the federation protocol.

`remote_tmux_focus_v1` is an additive peer capability on protocol 4. Older
protocol-4 peers remain compatible and retain outer-only focus. The hidden
`remote-focus` command accepts one bounded JSON request on stdin and returns a
typed selected-target confirmation or rejection. Its operation version is `1`;
unknown versions and malformed IDs are rejected before any tmux mutation.

Peer status contains connection state, a bounded error message, application
version, protocol, and capabilities. It intentionally has no freshness
timestamp because snapshots are emitted on state changes, not as connection
heartbeats.

The terminal UI keeps one `watch` connection open. The daemon releases that
connection when the UI closes or replaces its watch. The UI redraws after
input, resize, snapshot, message, working animation, or a visible unfinished
subagent's one-second elapsed-time change. A user-managed sidebar checks whether
its tmux window is active in an attached session. Hidden sidebars keep their
latest snapshot, search, and selection but pause drawing, working animation,
and elapsed-time ticks until the window is visible again. An explicit numeric
selection permits one hidden redraw so sibling pane buffers stay synchronized
without waiting for their next visibility probe. Visibility never changes
search, selection, scrolling, or activation state. Popups remain visible for
their whole lifetime, and `r` replaces the current watch with a freshly
connected stream and its initial snapshot.

Numeric shortcuts publish only their explicit selected agent ID to sibling
persistent UIs in the same tmux server. Activation focuses first; selection
fanout then runs in the background and wakes recipient panes concurrently.
Ordinary navigation and tmux focus changes remain local to each UI process.
Successful Enter, mouse, and numeric activation share the same focus seam. An
exact focus reports usage on a best-effort basis after tmux focus succeeds,
advancing the daemon watch so every persistent UI on the same tmux server
receives the reordered snapshot. A transport-only remote focus does not report
usage. An unavailable usage operation never turns a successful exact focus into
a failed activation.

## Plugin and managed-binary compatibility

The TPM checkout owns compatibility repair and fallback installation; the
packaged binary owns update, version listing, and rollback, while the managed
binary store owns executable versions. `COMPATIBILITY` defines the launcher
protocol and minimum binary version required by a checkout. Every published
managed version directory records its binary version and launcher protocol in
its own `COMPATIBILITY` metadata. Management-capable packages additionally
record management protocol `1`. The store has two independently atomic
selections: `current` is the runtime binary, while `manager` is the verified
lifecycle controller used only for update, version listing, and rollback.
Compatibility requires all of the following:

- `current` names the verified binary in its immutable `versions/<version>`
  directory;
- the binary reports the same semantic version as that directory and metadata;
- the installed and checkout launcher protocols are equal; and
- the binary version is at or above the checkout's minimum.

`manager` must use the same constrained relative or data-directory-absolute
target shape as `current`, name a native installed package with management
protocol `1`, and pass the same binary and metadata validation. Rollback changes
only `current`; keeping `manager` on a management-capable package ensures that
an older runtime without lifecycle subcommands cannot strand the installation.
Controller selection is monotonic: bootstrap and packaged update keep an
already verified `manager` when its semantic version is equal to or newer than
the candidate.

Bootstrap holds `~/.local/share/tmux-agent/.install.lock` (under the configured
data directory) while it rechecks compatibility, publishes a complete version
directory, and atomically renames a relative `current` symlink. Rollback uses
the same lock and Rust activation/restart-recovery operation as update. Under
that lock, bootstrap can add native target and launcher metadata to the exact
pre-self-update TPM layout: an in-store, metadata-absent `current` runtime with
no `manager`. It publishes `TARGET` first and `COMPATIBILITY` as the commit
point, so an exact `TARGET`-only interruption is rerunnable without granting
the legacy runtime management capability. Other partial or ambiguous layouts
fail closed. The
packaged `tmux-agent update`
command also uses this contract without reading a checkout: mutable GitHub
metadata is used only to discover and validate the latest stable semantic
version, while archive and `SHA256SUMS` downloads use immutable version-pinned
URLs. Release archives carry explicit target and launcher compatibility
metadata in addition to the binary version. All entries must be regular files
from a fixed allowlist and remain within transfer and expansion bounds before
an immutable version directory is published. Binary-version probes are also
time- and output-bounded.

The standalone installer publishes a small checkout-independent launcher that
routes runtime commands to `current` and lifecycle commands to `manager`. When it
finds a direct binary at the launcher path, bootstrap imports that binary under
the shared lock, records its native target and launcher protocol, and preserves
it as a recovery version before the launcher path is atomically replaced. The
same-version case is deferred until a checksum-verified package has been staged;
the direct binary must be byte-identical to that package. Store and version
directory collisions must be real in-store directories whose binary and
metadata files are regular non-symlinks. The launcher's exact versioned header
distinguishes an older official launcher, which can be replaced atomically
without treating it as a direct binary. The same header is required before
uninstall removes a configured standalone launcher path. The
checkout launcher retains compatibility repair for TPM, but its legacy version
listing and rollback commands delegate to `manager`. Normal daemon, UI, and
agent commands continue to execute `current`.

Thus an older checkout treats a newer binary with the same launcher protocol
as current and cannot replace it with the checkout-pinned fallback. A missing,
below-minimum, corrupt, or protocol-incompatible managed binary is repaired
from that pinned fallback. Self-update activates only a completely verified
version, restarts the daemon afterward, and restores the previous activation
if the restart fails. Managed listing and rollback fail closed when a selected
directory, binary, platform record, or compatibility record is missing,
incompatible, symlinked, or corrupt.

This launcher protocol is separate from the daemon federation protocol. TPM's
`prefix + U` continues to update the plugin checkout; it is not a managed-binary
update command.

The release lifecycle gate combines these component contracts without giving
one component another's authority. Rust tests exercise update discovery,
downgrade prevention, locking, verification, atomic activation, daemon restart
recovery, version selection, and rollback. Shell integration tests exercise
bootstrap and both launchers. The release workflow then uses the checksum-pinned
public v0.3.0 packages and a genuinely newer candidate package to exercise
fresh and upgrade layouts in standalone and TPM modes on every supported
release target. The v0.3.0 runtime remains `current` during TPM checkout
migration while the candidate becomes `manager`; lifecycle commands therefore
remain available after rolling back to a runtime that predates them.

## SSH federation

A structured machine produces a hardened command equivalent to:

```text
ssh -T -o BatchMode=yes -o ConnectTimeout=5 \
  agent@build-host.example.ts.net \
  /absolute/path/tmux-agent watch --jsonl --local-only
```

The central daemon verifies protocol compatibility, namespaces remote IDs with
the configured alias, merges derived records, and removes them when the stream
fails. SSH provides encryption, authentication, host keys, and routing. There
is no application TCP listener or shared application credential.

Remote lifecycle changes remain outside the federation protocol. A user may
run `tmux-agent update`, `versions`, or `rollback` through an ordinary explicit
SSH command, but neither the central daemon nor a configured peer can initiate
that command.

Federation snapshots never include captured pane contents, screen buffers,
prompts, reasoning, rollout events, goal objectives, or raw process command
lines.

## Focus

Local records focus their stored tmux session, window, and pane. Remote tmux
records carry the current attached clients for their owning server and session.
The scanner reads tmux server PID/start time, session ID/creation time, and live
client PIDs each scan. It joins client ancestry against the cached process
inventory. SSH clients use the established TCP endpoints of their sshd ancestor;
Mosh clients use the bound UDP endpoint of their mosh-server ancestor.

On Linux, OpenSSH can make its login process's file-descriptor table unreadable
to the logged-in user. When socket enumeration is unavailable, the scanner
extracts only `SSH_CONNECTION` from the current attached tmux client's login
ancestry. It requires a live sshd ancestor, consistent numeric endpoints, and
the exact established inbound connection in procfs. Mosh ancestry takes
precedence. Multiple outer terminals sharing that connection remain ambiguous.
Each ancestry member must predate the process-table scan and retain its parent
and start time through metadata extraction, preventing reused PIDs from joining
new connection metadata to old ancestry.
Missing or conflicting evidence leaves the attachment incomplete. This fallback
does not read session-global tmux environment or export process environments.

The local resolver matches those endpoints against live, non-UI transport panes.
Mosh's numeric client endpoint stays available after bootstrap SSH exits and
while its client address roams. No selected agent title, active provider,
inherited agent SSH environment, or persisted binding participates in this
association. A shell or editor in the active remote window therefore does not
hide the transport carrying agents in other windows. Named servers work when
the remote collector targets that server.

Focus and daemon reconciliation use the same unique association. Focus
rechecks local panes instead of trusting a precomputed target. Daemon labels
and visibility use that association too; visibility still requires both the
remote agent pane and local transport pane to be visible.

Each activation captures the local tmux socket, server lifetime, and current
client's name, PID, and creation time before selecting anything. A persistent
UI uses tmux's current/most recently active client at that moment, not a client
cached when the UI started. Selection uses that explicit client and numeric
session, window, and pane IDs. After asynchronous remote control, verification
checks the same client's selected location without switching again. When the
transport is in another local session, only that client switches sessions;
spectators remain in the original UI session. Within a shared session, native
tmux window selection still changes every attached client's view. Pane selection
is shared by default; clients using tmux's `active-pane` flag retain independent
pane selection. Exact focus still requires the captured client's actual pane
to match the target. Detach or user navigation during control fails verification
and is not undone. A CLI outside tmux continues to select and verify the target
session's active window and pane without switching an attached client.

For a structured machine advertising `remote_tmux_focus_v1`, activation sends
the server/session lifetime, session/window/pane IDs, and specifically matched
client endpoint over a separate configured SSH control command. The remote
operation uses its configured server, refreshes process/socket inspection,
validates the target and client association, selects the window and pane by ID,
and verifies the result and association again. It never switches a client to
another session. Tmux window selection affects all clients attached to that
session; shared-pane clients also share pane selection across linked views of
that window. Clients using `active-pane` retain independent pane selection. The
local side rechecks the same transport and initiating client's location after remote
confirmation before returning `Exact`. Display titles and labels do not form
part of that identity check.

Control uses bounded JSON input/output and a five-second total SSH deadline.
The authenticated configured machine establishes the host boundary; remote
server PID/start time and session creation time reject a different server or
reused session ID. Named servers must use the same remote configuration for
collection and control. No server discovery or command inference is attempted.

An older peer, raw collector, or uninspectable explicit binding still returns
`TransportOnly`, acknowledges an activated completion or goal achievement,
keeps popups open, and does not record last-used ordering. A rejected or failed
control operation reports that outer focus happened without confirmed inner
focus, keeps the popup open, and does not acknowledge or record usage.

Session switches update the attachment on the next scan. Detach removes it;
reconnect discovers the new client and endpoint. Process/socket changes can
take up to the one-second inventory refresh. Associations belong to the live
server/session lifetime and are rebuilt rather than persisted as markers.
Zero or multiple matching panes fail without a host-only or title fallback.

If a live attached client's ancestry or sockets cannot be inspected, an exact
explicit host/session binding remains available through `remote bind`, including
when other known clients have no matching local transport. The user maintains
that binding after session switches. When every attached client is inspectable,
no local match rejects old bindings. Address translation,
SSH proxy or multiplex arrangements that hide the matching endpoint, and tmux
clients nested beneath another tmux server may prevent automatic discovery.
With complete inspection, translated endpoints cannot be overridden with stale
unscoped bindings.
Install `lsof` on both machines for socket discovery.

Ordinary remote terminals retain their existing SSH endpoint or unique unmarked
Mosh destination/title matching. Compatibility records without attachment
metadata retain the older explicit-binding and title-based recovery paths.
Federation protocol 4 rejects older peers before those records are merged.

## Security boundaries

- Runtime directories use mode `0700` and files use mode `0600`.
- Federation transport remains SSH. An explicit focus binding may point at a
  local SSH or mosh pane.
- Captured pane contents remain on the owning machine.
- Normal federation does not transport child transcript content.
- Opening a remote Codex child uses a separate, explicit SSH viewer session.
- Ambiguous process, parent, or focus matches are rejected instead of guessed.
