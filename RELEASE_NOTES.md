tmux-agent v0.7.0 adds first-class Oh My Pi (OMP) support and a real terminal
demo of the tmux-agent workflow.

## Highlights

- OMP sessions are detected as a distinct provider in tmux and ordinary
  terminals without changing Pi detection.
- Owned-PTY OMP sessions report typed working, needs-input, idle, and done
  states from their visible terminal surface.
- Animated OMP activity titles are normalized to a stable task title, and OMP
  rows use a dedicated magenta provider badge.
- `tmux-agent omp [args...]` runs OMP inside tmux-agent's owned PTY while
  forwarding arguments, terminal behavior, and the child exit status.
- The README now includes a sanitized recording of a real tmux layout,
  demonstrating search, selection, and switching between Codex and OMP.

## Compatibility

- No configuration or federation protocol migration is required.
- OMP and Pi remain separate providers. Lookalike command names are not
  classified as either provider.
- Process-only OMP sessions in ordinary terminals remain evidence-limited
  `unknown` unless tmux-agent owns the inner PTY screen.
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
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.7.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.7.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.7.0/SECURITY.md).
