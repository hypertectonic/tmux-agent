# Dependency license audit

The authoritative dependency set is the locked graph in `Cargo.lock`.
tmux-agent itself is licensed under MIT. Release candidates also retain
`THIRD_PARTY_NOTICES.md` and the generated `THIRD_PARTY_LICENSES.html` report.

## Audit procedure

```sh
cargo install cargo-license --version 0.7.0 --locked
cargo install cargo-audit --version 0.22.2 --locked
cargo install cargo-about --version 0.9.1 --locked --features cli
cargo license --avoid-dev-deps
cargo audit
scripts/generate-third-party-licenses
scripts/check-third-party-licenses
```

Any dependency with an unknown, non-commercial, source-available, or copyleft
license requires explicit review. A lockfile change invalidates the evidence
below and requires a fresh audit and generated report.

## Current candidate evidence

Audit date: 2026-08-05

`Cargo.lock` SHA-256:

```text
896b3936743df2947092a65ed82067d65d0fff1858c30299253f5f1d6e50f430
```

`cargo-audit` 0.22.2 loaded 1,189 RustSec advisories and scanned 235 locked
crate dependencies with no known vulnerability reported.

`cargo-license` 0.7.0 reported no dependency with an unknown license. The
resolved graph is predominantly MIT, Apache-2.0, BSD, ISC, Unicode, Unlicense,
CC0, BSL, and Zlib terms or combinations that offer one of those permissive
choices.

Two uncommon declarations receive explicit review:

- `option-ext` 0.2.0 is MPL-2.0 and is used unmodified through `dirs-sys`.
  MPL-2.0 applies at the file level, and no dependency source is copied into
  this repository.
- `terminfo` 0.9.0 is WTFPL. It appears in the lockfile inventory through an
  optional dependency branch and is absent from the resolved production graph
  for the default build.

Dependencies that offer LGPL or MPL terms alongside MIT or Apache-2.0 provide
a permissive license choice used by this project.
