tmux-agent v0.10.2 restores goal and activity detection for current Codex and
Claude terminal interfaces.

## Fixes

- Recognize both single-line and two-line Codex goal footers and display
  token-budget progress without forwarding goal objectives.
- Recognize current Claude child navigation hints and main-row headers.
- Keep Claude's working indicator active while its validated status says it is
  waiting for background agents. This also works when child counters are hidden
  or frozen. Permission prompts and stale-content rejection remain in effect.

Outside that explicit waiting status, Claude child activity still requires
advancing counters and expires after two seconds without progress. This release
does not add separate Claude child rows or transcript collection.

Codex's budget-exhausted `Goal unmet` status is not yet supported.

## Compatibility

Federation remains on protocol 4. Handoff commands, skills and managed launcher
compatibility are unchanged. No extra polling loop is introduced.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
tmux-agent --version
```

Update configured remote machines explicitly, then the central machine.
Restart existing daemons, collectors and UI processes; replacing a binary on
disk does not replace running code. If returning from a private experiment,
restore official local and remote launch paths before restarting.

TPM's `prefix + U` updates the plugin checkout, not the packaged binary.

## Downloads and documentation

Archives are available for macOS Apple Silicon, macOS Intel, Linux x86-64 and
Linux ARM64. Verify archives with `SHA256SUMS`; release assets have signed build
provenance.

See the [installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.2/docs/installation.md)
and [remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.2/docs/remote-machines.md).
