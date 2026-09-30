tmux-agent v0.10.1 improves Claude activity detection when pane titles do not
reflect a live turn or a working child.

## Fixes

- Recognize live-turn progress above Claude's input prompt, including screens
  with auxiliary status lines. Missing or unrecognized footers do not imply
  activity.
- Confirm child progress across successive captures, including Claude's child
  navigation view. Frozen counters expire after a short grace period;
  foreground permission prompts still take precedence.
- Preserve process-start identity across inventory refreshes so confirmed
  child progress survives a refresh.

Child activity remains a screen-based estimate. Replacing a child between
captures with the same rendered label and larger counters can briefly imply
activity. Without further advances, it expires after two seconds of successful
captures. This release does not add child session IDs or transcript collection.

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

See the [installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.1/docs/installation.md)
and [remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.10.1/docs/remote-machines.md).
