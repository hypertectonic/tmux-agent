# Third-party notices

## Rust dependencies

tmux-agent links Rust crates recorded in `Cargo.lock`. Their declared licenses
include MIT, Apache-2.0, BSD-2-Clause, BSL-1.0, CC0-1.0, MPL-2.0, Unicode,
Unlicense, WTFPL, Zlib, and compatible dual-license combinations.

The reviewed license categories, exceptional terms, lockfile hash, and audit
procedure are documented in `docs/dependency-licenses.md`. Copyright and
license details for each exact crate are bundled in
`THIRD_PARTY_LICENSES.html` and can be regenerated from the release candidate
with:

```sh
scripts/generate-third-party-licenses
```

No dependency source has been copied into the tmux-agent source tree. Release
archives include this notice, the generated dependency license report, and the
project's MIT license.
