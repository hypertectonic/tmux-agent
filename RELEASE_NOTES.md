tmux-agent v0.4.1 is a maintenance release that strengthens the internal
boundary around Codex subagent ownership without changing the command surface,
protocol, UI, privacy model, or update flow.

## Highlights

- Process-owned Codex rollout discovery now lives behind the Codex evidence
  boundary instead of the generic tmux adapter.
- Codex ownership state and reconciliation now live behind a focused,
  deterministic module while generic process linking remains shared by every
  supported harness.
- Exact and resumed identity precedence, retained root bindings, nested
  ownership, deduplication, completion suppression, and fail-closed ambiguity
  remain covered at the focused seam.

## Compatibility

- No configuration, protocol, command, UI, privacy, or retention-timing changes
  are introduced in this release.
- Existing standalone and TPM installations can update through the normal
  verified lifecycle.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
```

TPM's `prefix + U` still updates the plugin checkout. It does not replace the
packaged `tmux-agent update` command. Updates on SSH machines remain ordinary
commands explicitly initiated by the user on each machine.

## Downloads

Signed build provenance is available for all release assets. Archives are
provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and Linux ARM64.
Verify downloaded archives with `SHA256SUMS` before installation.

## Documentation

See the
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.4.1/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.4.1/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.4.1/SECURITY.md).
