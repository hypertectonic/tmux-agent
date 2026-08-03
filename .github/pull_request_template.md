## Summary

Describe the user-visible change and its motivation.

## Validation

- [ ] `cargo fmt --all --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] Relevant shell and isolated tmux tests
- [ ] `scripts/check-public-tree`
- [ ] `scripts/check-third-party-licenses` when `Cargo.lock` changes

## Safety

- [ ] No pane contents were added to federation snapshots.
- [ ] SSH remains the only remote transport.
- [ ] Protocol changes are backward-compatible or increment the protocol.
- [ ] Documentation and user-facing text contain no em dash.
