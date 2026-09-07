tmux-agent v0.9.0 adds mark-all-read and quit confirmation, improves nested
remote tmux focus over SSH and Mosh, and fixes Claude discovery and idle state.

## Highlights

- Press `a` in normal UI mode to mark all currently unread completions as read.
- Press `q`, then `y` or `Y`, to close the UI. Press `Esc` to cancel.
- Claude task titles no longer include activity glyphs.
- Remote tmux focus matches live session attachments, including agents in
  hidden windows, then selects and verifies the requested inner window and
  pane through configured SSH control. Mosh remains supported after its
  bootstrap SSH connection exits.
- Remote focus preserves the initiating local client and rejects stale or
  ambiguous identities. Outer-only focus is reported explicitly instead of
  being presented as a successful inner selection.
- Completed Claude turns remain idle when persistent background shells are
  running. Active-turn and permission-prompt signals retain their precedence.
- Claude native versioned entrypoints are recognized even when the process
  name is a version number.

## Compatibility and remote setup

Federation changes from protocol 3 to protocol 4. Update every connected
machine together; older protocol peers cannot federate with this version.
The managed launcher protocol remains unchanged.

Use structured `[[machine]]` configuration and install `lsof` on both machines
for live transport discovery and inner selection. The remote `watch` and
`remote-focus` commands must use the same updated binary and tmux-server
configuration. Restart existing collectors and UI processes after updating,
as well as the daemons. A new binary on disk does not replace a running process.

Raw collectors and uninspectable explicit bindings retain reported outer-only
focus. Ambiguous transports fail closed. Exact focus does not support tmux
clients using the `active-pane` flag. Shared tmux sessions still share window
selection; this release does not change tmux's sharing rules.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
tmux-agent --version
```

Update each remote explicitly, then update the central machine and restart its
daemon and existing UIs so new collectors run the updated remote command.
Verify peer connectivity and test selection of the intended inner window and
pane. Updates on SSH machines remain ordinary commands initiated by the user;
tmux-agent does not orchestrate remote lifecycle changes.

TPM's `prefix + U` updates the plugin checkout, not the packaged binary.

## Downloads

Archives are provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and
Linux ARM64. Verify downloaded archives with `SHA256SUMS`. Release assets have
signed build provenance.

## Documentation

See the
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.9.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.9.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.9.0/SECURITY.md).
