# cctop's fork of rmux

This is [Helvesec/rmux](https://github.com/Helvesec/rmux), forked for
[cctop](https://github.com/flolep2607/cctop), which drives its agents through
an rmux daemon. The plan and its reasons are in flolep2607/cctop#197.

## What differs from upstream

- **Published names.** The crates cctop builds on go to crates.io as
  `cctop-rmux-*`: `cctop-rmux-types`, `-proto`, `-os`, `-ipc` and `-sdk`, which
  cctop's queries use, and `-core`, `-pty`, `-web-crypto`, `-server` and
  `-client`, which are the daemon and attach client cctop builds in.
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
- **For an embedded daemon.** Three additions, all for a program that ships the
  server inside its own binary and must never reach anybody else's daemon:
  `RmuxBuilder::connect_or_start_with` starts a daemon from a command the
  caller gives, on an explicit socket, with nothing discovered from the
  environment or `PATH`; `DaemonConfig::without_tmux_shim` keeps the `tmux`
  shim (which runs an `rmux` binary) off its panes' `PATH`; and
  `WEB_SHARE_PROTOCOL_VERSION` is public, so a pinned copy of the share page
  can be tested against the server it ships with.
- **Upstream pull requests merged early.** Helvesec/rmux#227, the Kitty
  keyboard protocol's disambiguate flag, which lets a pane tell Shift+Enter
  from Enter (Claude Code's newline). Merged as upstream's own commits, so
  upstream taking it later merges without conflict. Not taken: #226 (popups
  blank before painting), since cctop opens no popups, and #229 (the
  `RmuxShell` trait), since cctop runs its panes' commands as they are.
- **No `rustix::runtime`.** The client's SIGWINCH watcher blocks, waits for
  and sends the signal through libc. rustix 1.1.5 made its `runtime` module
  (documented as being for libc implementations) crate-private, which broke
  the client wherever 1.1.5 was resolved; upstream still uses it, and the fix
  is one to send back.
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
