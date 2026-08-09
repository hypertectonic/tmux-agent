tmux-agent v0.4.0 adds secure checkout-independent updates, managed rollback,
and more reliable Codex subagent ownership.

## Highlights

- Update an official installation with `tmux-agent update` without keeping or
  locating a source checkout.
- Inspect installed recovery versions with `tmux-agent versions` and select a
  verified previous version with `tmux-agent rollback <version>`.
- Keep runtime and lifecycle-controller selection independent, so rolling back
  to an older binary does not strand future update or recovery commands.
- Migrate supported standalone and pre-self-update TPM installations without
  discarding the previous binary or downgrading a newer compatible runtime.
- Keep subagents attached to the correct picker-resumed Codex session when
  multiple Codex sessions have used the same working directory.

## Security and reliability

- Release discovery is separated from immutable version-pinned downloads.
  Archives are checked for size, checksum, target, compatibility, embedded
  binary version, and a fixed file allowlist before atomic activation.
- Update and rollback share an installation lock, preserve the previous
  activation on failure, and restore it if daemon restart fails.
- Direct-binary and legacy TPM migrations reject ambiguous, symlinked,
  partially published, or mismatched layouts instead of guessing.

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
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.4.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.4.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.4.0/SECURITY.md).
