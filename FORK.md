# cctop's fork of rmux

This is [Helvesec/rmux](https://github.com/Helvesec/rmux), forked for
[cctop](https://github.com/flolep2607/cctop), which drives its agents through
an rmux daemon. The plan and its reasons are in flolep2607/cctop#197.

## What differs from upstream

- **Published names.** The crates cctop builds on go to crates.io as
  `cctop-rmux-*`: today `cctop-rmux-types`, `-proto`, `-os`, `-ipc` and `-sdk`.
  Only `[package] name` changes. Each keeps its upstream library name, and the
  dependency keys keep the upstream names through `package =` in the root
  `[workspace.dependencies]`, so the code still says `use rmux_sdk::…`.
- **Linux only**, like cctop. The Windows-only files of the published crates
  and their `cfg(windows)` dependency tables are gone. The inline `cfg(windows)`
  branches inside shared files stay, so upstream merges stay small; they are
  never compiled.
- **Trimmed.** `rmux-render-core`, `ratatui-rmux` and `xtask` are out of the
  workspace, as is upstream's release tooling (the release workflows,
  Chocolatey, snap, the Nix flake, the packaging scripts). The `rmux` CLI
  package stays, unpublished, as the daemon the SDK's integration tests drive.
  `rmux-core`, `-pty`, `-web-crypto`, `-server` and `-client` stay unpublished
  until cctop builds the daemon in.
- **Toolchain.** Pinned to cctop's channel (`rust-toolchain.toml`).

## Releasing

`tools/release-plan.sh` holds the rules, copied from cctop's: each published
crate has its own version and is bumped only when it changed since the last
`cctop-v*` tag; requirements between them are carets on the compatible part of
the version; a crate whose public API broke (cargo-semver-checks) takes a
breaking bump. To release, bump the crates that changed and
`[workspace.metadata.cctop-release] version` in the root `Cargo.toml`, and
merge. `.github/workflows/release.yml` then runs CI, publishes what crates.io
does not have yet in dependency order with the `CARGO_REGISTRY_TOKEN` secret,
and tags `cctop-v<version>`.

Versions started at upstream's 0.10.0. An upstream merge that moves the minor
moves ours.

## Upstream

Pull upstream with a merge, not a rebase, so the history stays shared. Send a
fix back to Helvesec/rmux from a branch without the rename commits.
