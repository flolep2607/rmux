# Live handover: upgrading a running daemon without its panes noticing

Status: design, with a feasibility prototype. Nothing here is implemented yet.

## Why

cctop runs every coding agent inside one long-lived rmux daemon:
`cctop mux daemon --socket $XDG_RUNTIME_DIR/cctop/mux/w<WIRE>/default`. The daemon
comes from whichever cctop binary was current when it started, and it runs for
days.

- **A new server feature reaches agents only once a new daemon runs.** OSC 7501
  program status, in cctop-rmux-server 0.10.2, is the latest example. Today a new
  daemon starts only after the old one holds no sessions, and for a busy user that
  never happens. `upgrade.rs` restarts a stale *idle* daemon and only warns about a
  stale *active* one.
- **A change to `RMUX_WIRE_VERSION` hides the old agents.** A new cctop starts a
  second daemon in `w<N+1>/`, and the agents in `w<N>/` disappear from its view.

The goal is a daemon that hands every session to its successor. Agent processes
keep running, their ptys keep working, and screens, scrollback, sizes, layout,
options, environment and names all come through. Clients reattach. The same
mechanism should work across a wire-version change.

## Recommendation in one paragraph

**Re-exec in place.** The daemon quiesces, writes its whole state into a memfd,
clears `FD_CLOEXEC` on the pty masters and the listening socket, and `execve`s the
new binary with `--resume-fd`. The pid stays the same, so every agent is still its
child and can be `waitpid`ed. The fds survive, so the ptys and the listener never
close. Output an agent writes during the exec waits in the pty, and a client that
connects during the exec waits in the listen backlog.

Two layers make it safe:
- **Verify before committing.** The new binary is first run as a separate process
  that only parses the state, and the daemon execs only if that succeeds.
- **Roll back after committing.** The daemon keeps an fd on its own executable.
  If the new image still fails to resume, it `execveat`s back into the old one
  with the same state.

The alternative, a two-process handover over SCM_RIGHTS, loses exit statuses: a
non-child cannot be `waitid`ed, as shown below. It also leaves agents reparented
to init, and it has two live owners of every pty during the transfer.

---

## 1. How rmux holds a pane today

All paths are under `crates/`.

### pty master

- **Opening the pair.** `rmux-pty/src/backend/linux.rs:17` `open_pty_pair()` opens
  the master with `openpt(RDWR|NOCTTY|CLOEXEC)` and the slave with
  `TIOCGPTPEER(...|CLOEXEC)`. `PtyPair::open` (`pty.rs:486`) sets the master
  `O_NONBLOCK`. That flag lives on the open file description, so it survives exec
  and SCM_RIGHTS.
- **The wrappers.** `PtyIo { fd: Arc<OwnedFd>, startup_slave }` (`pty.rs:61`) sits
  inside `PtyMaster { io, startup_slave }` (`pty.rs:308`). `PtyMaster::try_clone`
  shares the `Arc`. `PtyMaster::into_owned_fd()` (`pty.rs:403`) is a ready-made
  extraction point.
- **Async I/O.** `rmux-server/src/pane_io/wire.rs:84` `open_pane_writer` wraps the
  master in `tokio::io::unix::AsyncFd<PtyIo>`. There is one reader task per pane,
  `read_pane_output` (`pane_io/reader.rs:385`), on `PaneReaderRuntime`. There is no
  writer task: send-keys clones the master for each write
  (`pane_terminal_store.rs:221`).

### Child pid and reaping

- **Spawning.** `rmux-pty/src/child.rs:363` `spawn_child` uses
  `std::process::Command`. Its `pre_exec` resets signal dispositions, then runs
  `setup_child_controlling_terminal` (`linux.rs:60`): `setsid`, `TIOCSCTTY`,
  `tcsetpgrp`. Each agent is therefore a session leader whose pid is also its
  pgid.
- **Where the pid lives.** `PtyChild { child: std::process::Child, pid }` is at
  `child.rs:167`. The server wraps it in `PaneTerminal { master, child,
  exit_status, termination_attempted, runtime_window_name, profile }`
  (`rmux-server/src/pane_terminal_process.rs:22`), stored as
  `PaneTerminalStore.sessions: HashMap<SessionName, HashMap<PaneId,
  PaneTerminal>>` (`pane_terminal_store.rs:19`).
- **`impl Drop for PaneTerminal`** (`pane_terminal_process.rs:174`) runs
  `terminate_with_bounded_grace`: SIGHUP, then SIGKILL. **A handover must never
  drop a `PaneTerminal`.** It has to `mem::forget` it or disarm it.
- **Reaping.** It is `Child::try_wait()` (waitpid WNOHANG) only. Nothing uses a
  pidfd or a blocking waitpid loop.
  - SIGCHLD (`rmux-os/src/signals.rs:233`, self-pipe) only drives
    `continue_stopped_panes` (`listener_signals.rs:48`).
  - Exit is noticed when the reader gets EOF/EIO on the master. The reader
    publishes `PaneExitEvent::eof_published`, and then
    `handler_pane/lifecycle.rs:150` `handle_pane_exit_event` →
    `observe_runtime_pane_exit` calls `try_wait` with retries.
  - `should_keep_dead_pane` (`lifecycle.rs:882`) applies `remain-on-exit`.
    cctop sets it to `failed`, which needs the **exit status**.
  - A dead pane's `PaneExitMetadata { status, signal, time }` goes to
    `HandlerState.dead_panes`.
- **A `std::process::Child` cannot be rebuilt from a pid.** Whatever replaces it
  after a handover must be a raw pid (see §3).

### Terminal state

- **The layers.**
  - `PaneTranscript` (`rmux-server/src/pane_transcript.rs:45`, shared as
    `Arc<Mutex<_>>`) holds `terminal: TerminalScreen`, the copy-mode `mode`,
    `output_sequence` and clock/timer fields.
  - `TerminalScreen { parser: TerminalParser }` (`rmux-core/src/terminal_screen.rs:20`).
  - `TerminalParser { parser: InputParser, screen: Screen }` (`rmux-core/src/terminal/mod.rs:79`).
- **`InputParser`** (`rmux-core/src/input/mod.rs:98`) holds:
  - VT state machine: state, flags, intermediate/param/input buffers
  - SGR `CellState` and saved state
  - UTF-8 accumulator
  - kitty keyboard screens, `last_char` (for REP)
  - OSC 10/11/12 colours
  - OSC 7501 `ProgramStatusRecords` (`program_status.rs:315`)

  `pending_bytes_ref()` (`input/mod.rs:282`) returns the raw bytes of an
  unfinished sequence.
- **`Screen`** (`rmux-core/src/screen.rs:40`) holds:
  - `grid`, and `saved_grid` for the alternate screen
  - cursor, saved cursor, scroll region
  - `mode`, cursor style
  - title, `title_stack`, window name, OSC 7 `path`
  - tabs, hyperlinks, bell count, utf8 config
- **`Grid`** (`rmux-core/src/grid.rs:139`) holds `sx, sy, hlimit, history:
  VecDeque<GridLine>, visible: VecDeque<GridLine>` and history-stamp fields.
  - `GridLine { cells, plain_text, flags, time, revision }` is at
    `grid/cell.rs:413`.
  - `GridCell { text, width, flags, attr, fg, bg, us, link }` is at
    `grid/cell.rs:147`.
- **Serialisation support.** rmux-core has **no serde dependency**.
  - `Screen`, `Grid`, `GridLine` and `GridCell` are `Clone`.
  - `InputParser` and `TerminalParser` are not.
  - The only serde types are the wire DTOs in rmux-proto (bincode).

### Formats that already exist, and why none of them is the handover format

| Format | Where | Captures | Loses |
|---|---|---|---|
| Recovery keyframe (`PaneRecoverySeed`/`Draft`/`Keyframe`) | `rmux-server/src/pane_recovery.rs:54–112`, `materialize_keyframe` :414 | An ANSI repaint: title and title stack, OSC 7, OSC 10–12, the alt and main screens, scroll region, DEC modes, tabs, DECSC, cursor, SGR, then `pending_bytes` | Capped at 2 MiB and 4096×2048, with **scrollback only as far as the budget allows**. Also loses per-line timestamps, program status, REP `last_char`, the kitty stack and copy mode |
| `PaneSurfaceSnapshot` / `PaneSnapshotCell` | `rmux-proto/src/response/pane.rs:592,734` | Visible cells, cursor, title, path, colours | History and modes |
| `capture-pane -e` | `rmux-server/src/handler_buffer/capture_format.rs:100` | Text and SGR | Cursor, modes, alternate screen |
| `pane_state_journal.rs` | | Title, options and foreground events | The screen |

The keyframe is close in spirit, and two of its parts are worth reusing:
`render_dec_modes_for_snapshot`, and the idea of replaying `pending_bytes` into a
fresh parser. As a container it is a lossy repaint, though. A handover has no
budget to fit and must be exact, scrollback included (cctop sets
`history-limit 50000`).

### Everything else the daemon holds

- **`RequestHandler`** (`rmux-server/src/handler.rs:356`) is about 50 `Arc`s
  around `state: Arc<tokio::Mutex<HandlerState>>`.
- **`HandlerState`** (`rmux-server/src/pane_terminals.rs:267`) holds:
  - `sessions: SessionStore` (`rmux-core/src/session/store.rs:22`): sessions →
    `Session` (`session.rs:43`) → `windows: BTreeMap<u32, Window>` (`window.rs:81`:
    panes, `layout_tree: Option<LayoutTree>`, zoom, alert flags, name) → `Pane`
    (`pane.rs:50`: id, index, geometry, timestamps). It also holds the id
    counters.
  - `options: OptionStore`, `environment: EnvironmentStore`, `hooks: HookStore`,
    `buffers: BufferStore`, `key_bindings: KeyBindingStore`, `message_log`.
  - Per pane: `terminals`, `transcripts`, `pane_outputs` (the `OutputRing`
    broadcast, `pane_io/types.rs:336`), `pane_output_readers` (an `AbortHandle`,
    aborted on drop), `pane_lifecycle` (`PaneLifecycleState`:
    command, cwd, respawn env, generation, `Running{pid}`/`Exited`),
    `dead_panes`, `pipes` (pipe-pane children), `marked_pane`.
  - Window link groups and slots.
- **Clients** are runtime only:
  - `ActiveAttachState.by_pid` (`handler_attach/state.rs:26`) holds key tables,
    prompts, overlays and an mpsc to the connection task.
  - `ActiveControlState` is at `handler_control.rs:60`.
  - SDK `subscriptions`, `sdk_waits` and `session_leases` are also runtime.
  - An attach client that sees EOF without the stop sequence exits 1:
    `rmux-client/src/attach.rs` ~664, "attach stream closed before attach-stop
    sequence".
  - Nothing reconnects.
- **Web share.** `WebShareRegistry` (`rmux-server/src/web/registry.rs:133`) runs
  its own TCP listener (`web/server.rs:56`), keeps encrypted browser sessions with
  per-connection keys, and may run a tunnel subprocess.
- **The listening socket.** `bind_unix_listener_at`
  (`rmux-server/src/unix_socket.rs:51`):
  - creates a 0700 directory, removes a stale socket after a probe, binds via
    tokio (`SOCK_CLOEXEC`), chmods to 0600, and records the dev/inode.
  - `SocketCleanup` (`socket_cleanup.rs`) unlinks only if the dev/inode still
    match.
  - There is no pidfile.
- **Signals.** `rmux-os/src/signals.rs` installs INT, TERM, CHLD, USR1, HUP, QUIT
  and USR2 handlers with a self-pipe.
  - INT/TERM shut down, and USR1 rebinds the socket.
  - HUP, QUIT and USR2 are currently ignored.
- **Runtime.** tokio multi-thread (`src/server_runtime.rs:24`).
- **Daemon spawn.** `rmux-os/src/daemon.rs:236` is one fork, with every fd
  CLOEXEC except the ready eventfd, then `setsid`. cctop instead spawns
  `/proc/self/exe mux daemon --socket …` itself (cctop `crates/core/src/mux.rs`
  `start`) and never waits on it.
- **Version handshake.**
  - `RMUX_WIRE_VERSION = 8` is at `rmux-proto/src/envelope.rs:10`. Every frame
    carries magic `0x52` plus the version.
  - `DaemonStatus` (`handler_daemon.rs:12`) returns `rmux_version`, `wire_version`
    and session and client counts.
  - `upgrade.rs:52` `inspect_daemon` classifies the daemon as `Current`,
    `StaleIdle`, `StaleActive` or `Incompatible`.
  - `upgrade_restart.rs:39` handles `StaleIdle` with `ShutdownIfIdle` →
    `PendingShutdownReason::SeamlessUpgradeIdle` (`handler.rs:238`).
- **Re-exec.** Nothing in the codebase re-execs the daemon.

---

## 2. The handover protocol

### Who starts it, and the request channel

**The new binary asks, and the old daemon execs into it.** No third process ever
owns a pane.

**The trigger in cctop.**
- **On V-switch (`crates/ui/src/switch.rs`) and at TUI startup.** The new TUI
  sends `DaemonStatus`. If `rmux_version` is older than the cctop-rmux-server it
  was built with, the TUI asks for a handover *before* `restore_running_tabs`
  attaches anything. At that moment cctop's own tabs are not attached, because
  the old TUI and its attach children have just exited. The only clients left
  are other TUIs, `cctop serve` and web shares.
- **After a plain `cctop --update` with no TUI running.** Nothing happens until
  the next TUI starts. A `cctop mux handover` subcommand covers scripts and
  `doctor --fix`.

**The request travels in a versionless preamble on the existing socket, not in a
wire-N frame.**

A wire-N+1 client cannot speak to a wire-N daemon, but the cross-wire case is
exactly when a handover is needed most. So the listener sniffs the first bytes of
each connection, before the versioned codec. A connection that opens with
`RMUXHO1\n` instead of the `0x52` frame magic carries one JSON line, and gets one
JSON line back. The preamble and its JSON are **frozen forever**; new fields are
optional.

```json
→ {"op":"status"}
← {"ok":true,"pid":1234,"rmux_version":"0.10.3","wire":8,"handover_schema":1,"sessions":7}

→ {"op":"handover","listen":"/run/user/1000/cctop/mux/w9/default",
   "argv_tail":["mux","daemon"],"accept_schema":[1,2]}
← {"ok":true,"phase":"verified"}   … then the connection closes, and the daemon execs
← {"ok":false,"error":"web share active", "retry":false}
```

**Authentication.**
- The socket is already 0600, in a 0700 directory.
- The daemon also checks `SO_PEERCRED`: the peer's uid must be the daemon's.
- **The exec target is never a path from the request.** The daemon opens
  `/proc/<peer pid>/exe` (`O_PATH`) and later `execveat`s that fd. A process can
  only make the daemon run the binary that process is itself running. That
  removes the TOCTOU, and it makes "upgrade to the binary that asked" the only
  possible outcome.
- The peer pid is pinned with `SO_PEERPIDFD` where the kernel has it (6.5+), and
  with `/proc/<pid>/stat` starttime otherwise.
- The argv is the embedder's (`argv_tail`, plus `--resume-fd N` and `--socket
  <listen>`). For cctop that is `cctop mux daemon --socket … --resume-fd N`.
  rmux's own binary would use `--__internal-daemon … --resume-fd N`.

### The old daemon's sequence

1. **Admission.** It refuses with a reason, and nothing changes, if any of these
   is true: a handover is already running; `accept_schema` does not include its
   own schema; the peer fails authentication; or an unsupported feature is active
   (see "Scope" below). This is cheap and changes nothing.
2. **Quiesce.** This reuses the shutdown path's first steps (`listener.rs:189`):
   - Close request admission. New connections queue in the kernel backlog, which
     the prototype shows survives exec.
   - Let in-flight commands, hooks and the startup config task finish against a
     deadline, as `serve` already does.
   - Stop each pane reader **at the top of its loop**. The loop has three await
     points (`read_from_pane`, the parser-reply write, and `yield_now`). Aborting
     the task could drop a buffer that was read but not yet parsed. Instead, a
     per-reader `watch` gate is selected against `read_from_pane` only.
   - Once every reader is parked, everything read is parsed, and everything
     unread is still in the kernel's pty buffer.
3. **Snapshot.** It takes `HandlerState` once, without awaiting, and serialises
   it into a sealed memfd (§2.2, `F_SEAL_WRITE|SHRINK|GROW`).
4. **Verify.** It spawns the new binary as a **separate process**: `execveat(peer
   exe fd) … --handover-verify <memfd>`.
   - This process parses the whole state and checks every invariant (each pane
     has an fd slot, window/pane ids are unique, layouts fit the window sizes).
   - It exits 0 or prints why not.
   - It touches no fd and no socket.
   - Nothing is committed yet. On failure the daemon drops the memfd, un-parks the
     readers, reopens admission, and answers `ok:false`. **The old daemon carries
     on untouched.**
5. **Tell clients** (see below), then answer `{"phase":"verified"}`.
6. **Commit.**
   - Clear `FD_CLOEXEC` on: the memfd, each pty master, the unix listener, the web
     listener (if handed over), and an `O_RDONLY` fd on its own `/proc/self/exe`
     (kept for rollback).
   - Reset the signal mask.
   - `mem::forget` the `PaneTerminal`s and the `SocketCleanup`.
   - `execveat(peer_exe_fd, "", argv, envp, AT_EMPTY_PATH)`.

   All other threads vanish with the exec. That is why step 2 had to leave no
   thread in the middle of a write.

### The new image's sequence

- Parse the memfd. Rebuild `HandlerState`, with each pane's master `OwnedFd` from
  the fd number in the state and each child as `PtyChild::Adopted { pid }` (§3).
- Restart one reader per pane, and bind or adopt listeners (§4).
- Run one reap sweep: `waitpid(pid, WNOHANG)` for every adopted pane. This
  catches an agent that died during the exec, whose zombie keeps its pid
  reserved, so there is no reuse race.
- Readers find the bytes written during the exec waiting in the pty, which the
  prototype confirms.
- **On any error before readiness:** `execveat(old_exe_fd, … --resume-fd N)` back
  into the old binary. That works even though its file has been deleted, which the
  prototype also confirms. The old image restores from the same state, in a
  format it wrote itself.
- The daemon logs the error and writes `handover.failed` (reason, versions)
  beside the socket. `cctop doctor` reports it.

### 2.2 The state format

- **A dedicated, versioned schema of DTOs**, `rmux-server/src/handover/` with
  `HandoverStateV1`, bincode-encoded. It is never `#[derive(Serialize)]` on core
  internals.
  - Internals (`Grid`, `Session`, `LayoutTree`) can then change freely.
  - Only the DTO conversion has to keep up.
  - An explicit `handover_schema: u32` is checked by `accept_schema`.
  - A new binary reads the current schema and the one before it. Since the
    requester is the new binary, it announces what it reads, and the old daemon
    can refuse up front.
- **rmux-core** grows `pub` constructors and accessors, or `to_handover` /
  `from_handover` behind a `handover` feature. Its fields are private today, and
  `LayoutTree` is `pub(crate)`.
- **Header:** schema, writer `rmux_version`, wire version, and the fd table, which
  maps each `PaneId` to an fd number.
- **Session tree:** `SessionStore` (sessions, groups, id counters, recency),
  windows (index, name, automatic-rename state, layout tree, zoom, alert flags,
  size), panes (id, index, geometry, timestamps), and window link groups.
- **Stores:** `OptionStore` (all scopes, including user `@options`),
  `EnvironmentStore` (global and per-session), `HookStore`, `KeyBindingStore`,
  `BufferStore`, `message_log`, `marked_pane`.
- **Per pane:**
  - `PaneLifecycleState`: command, cwd, respawn env, generation, pid,
    `output_sequence`.
  - `dead_panes` metadata.
  - `input_disabled`.
- **Terminal per pane:** the `Screen` in full, as a DTO mirror.
  - Grid: history and visible lines with cells, flags, times and `hlimit`.
  - `saved_grid` (the alternate screen).
  - Cursor and saved cursor, scroll region, modes, tabs, title and title stack,
    path, hyperlinks, colours.
  - From the parser: SGR `CellState`, saved state, kitty screens, `last_char`,
    OSC colours, program-status records, and `pending_bytes`. The resumed parser
    starts in ground state and is fed `pending_bytes`, as the recovery keyframe
    already does. This avoids serialising the VT state machine itself.
- **Not carried:** copy-mode (`PaneModeState`), which simply leaves copy mode;
  the `OutputRing` history, since clients redraw; timers; prompt history; and
  every per-client struct.

**Size.** 50 000 history lines × 200 columns × 20 panes is a worst case of
2×10⁸ cells. Lines whose cells all share default attributes should be stored as
`(text, run-length attrs)` rather than cell by cell. That keeps a typical state
in the tens of MB, which a memfd holds trivially. The write is in-memory and
should take well under a second; measure it with step 1 of the plan.

### Taking the listening socket over atomically

- **Same wire version:** nothing to take over. The listener fd survives the exec
  with its path and its backlog. A client that connects during the exec is
  accepted afterwards, as the prototype shows. `SocketCleanup`'s dev/inode guard
  still matches because it is the same socket.
- **The two-process alternative,** if it were ever chosen: the new daemon binds
  `default.new` and `rename(2)`s it over `default`. A new `connect` lands on the
  new listener, while the old listener keeps the clients it already accepted (also
  shown below). There is never a moment with no socket.

### Attached clients

Clients are the one thing that does notice. Attach and control connections are
plain sockets with framing state and per-connection tasks, which are cheaper to
re-establish than to serialise.

- **Phase 1 (enough for cctop).** Before committing, the daemon sends every attach
  client the normal stop sequence with a new reason, `[server upgrading]`, and
  control clients `%exit server-upgrade`. Clients exit cleanly with status 0.
  - cctop tabs treat that reason like a transient detach and re-run `cctop mux
    attach NAME` once the socket answers.
  - On V-switch there are usually no cctop attach clients left by then (see
    above).
  - SDK request connections (`DaemonStatus`, list, send-keys) get EOF and are
    retried by their callers.
- **Phase 2 (rmux users).** A new capability, `attach.reconnect`. The stop frame
  carries the socket to reconnect to, and the attach client reconnects to the
  same session and target with its terminal untouched, so the user sees one
  redraw. Old clients without the capability get phase 1.

### Scope: what refuses a handover in v1

Admission refuses, and the old daemon keeps running, when any of these is active:
- a **web share**, whose browser sessions hold per-connection keys
- a **pipe-pane** (its own child and tasks)
- a running **`run-shell -b`** or status job
- a **detached SDK lease**

Each one can be lifted later. For example, the web listener fd can be carried
like the unix one, with shares re-issued from persisted records. cctop uses none
of them for agents. If `cctop serve` holds web shares, its tile frames reconnect
already.

### Failure and rollback

| Fails at | Effect |
|---|---|
| Admission, quiesce, snapshot or verify | The old daemon resumes readers and admission. No client was told anything. `ok:false` with the reason |
| The `execveat` itself (ENOEXEC, ENOMEM) | It returns, so the old image is still there. Un-forget, resume, and report |
| The new image fails to parse or rebuild | `execveat(old_exe_fd)` back with the same memfd. The old binary resumes its own schema |
| The new image panics after it is ready | Same as any daemon crash today. A `panic = abort` hook should `execveat` back while the fds are still intact (best effort) |
| The old image also fails to resume after rollback | The process dies. The masters close, and agents get SIGHUP. **This is the residual risk.** It needs two independent bugs: the verify process must pass a state the new image then rejects, *and* the old binary must fail to read its own format. CI guards both with round-trip tests |

---

## 3. The process-reaping problem

Prototype results on Linux 6.18 (WSL2):

| Question | Result |
|---|---|
| `pidfd_open` on a **non-child** | Works |
| `poll(pidfd)` while it runs / after it exits | `0` / `POLLIN` within ~130 µs of the signal. **A non-child's exit is observable** |
| `waitid(P_PIDFD, …)` on a non-child, running or exited | `ECHILD` both times. **Its exit status is unobtainable.** The real parent (or init) reaps it |
| `PR_SET_CHILD_SUBREAPER` | An orphaned grandchild was reparented to the subreaper, which then got `waitid(P_PIDFD)` → `CLD_KILLED`, SIGTERM. **It only helps an ancestor**, and the new daemon is not an ancestor of the old daemon's children |
| After the old daemon exited (two-process) | The agent was reparented to pid 379, WSL's `Relay`, or systemd/init elsewhere. Its status is lost to us |
| Across `execve` (re-exec) | The child's ppid is unchanged, and `waitpid(child)` in the new image returned its pid with exit code 0 |

What this means for each option:
- **pidfd over SCM_RIGHTS.** Good enough to *notice* an exit, but not to know how
  the agent died. cctop runs with `remain-on-exit failed`, which keeps a pane only
  when the agent failed. That is exactly the case it cares about, and without the
  status it cannot decide. A handed-over pane would also report a "dead" status it
  made up.
- **Subreaper.** It would require the new daemon to be started *by* the old one,
  and the old one to then exit. That is a re-exec with extra steps and a new pid.
- **Re-exec in place.** Nothing to solve. Children stay children. The only change
  is that `PtyChild` gains a variant, `Adopted { pid }`, reaped with raw
  `waitpid(pid, WNOHANG)` where `Child::try_wait` was used. A pidfd from
  `pidfd_open(pid)` can drive the async wakeup if wanted. The next spawn after a
  resume uses `Command` as today.

**Recommendation: re-exec in place.** What the two-process design does better, and
why each point does not decide it:
- **Rollback is trivial**, because the old process is never replaced. Re-exec
  gets nearly the same guarantee from the verify subprocess plus `execveat` back.
- **The new daemon gets a clean process** (environment, rlimits, cgroup). For
  cctop the old daemon's environment is already sanitised by `mux.rs` `start`
  (it strips `FOREIGN`), and keeping the agents' cgroup is if anything a feature.

Re-exec also avoids two owners of every pty and a reparenting to init. Same-wire
takeover needs nothing at all, because the listener never closes.

What re-exec carries over from the old process, and how to handle it:
- **The pid and the parent.** For cctop the parent is the TUI that started the
  daemon and never waits on it, a "ponytail" in `mux.rs`. This is unchanged.
- **Ignored signals and the signal mask.** Reset both explicitly at resume.
- **The environment.** The resumed image re-reads only what it needs from the
  state. It does not trust its own `envp` for anything session-related, since the
  `EnvironmentStore` is in the state.
- **`/proc/self/exe`** now points at the new binary. Good.

---

## 4. Wire-version changes

Re-exec handles a wire change with one extra field. The request carries `listen`,
the new socket path (`…/w9/default`), and the resumed image:

1. Binds the new path normally, as `bind_unix_listener_at` does.
2. Keeps the inherited `w8` listener for a short grace period, answering only the
   versionless preamble with `{"moved":"…/w9/default"}`. A wire-8 frame gets the
   existing `Incompatible` handshake answer.
3. Then closes the `w8` listener, unlinks its socket (the dev/inode guard still
   applies), and leaves `w8/moved` holding the new path for `doctor`.

The panes, their pids and their fds are untouched by any of this. Only the
protocol the clients speak changes, and the new image speaks the new one.

**Discovery.**
- When a cctop finds no daemon at `dir()` (`w<N>`), it scans sibling `w*/default`
  for daemons answering the preamble.
- If one is older, it asks that daemon to hand over into `w<N>/default`.
- This is the only cross-directory step. Because the preamble is versionless, it
  is the same call across any wire change.
- The JSON preamble must never change incompatibly. Its tests should pin the
  bytes.

**Bootstrapping.** Daemons that predate the preamble, including every 0.10.x
running today, cannot be handed over. They need one last drain, or a manual
restart. After that, every later version is covered.

---

## 5. Prototype evidence

The source is a throwaway Rust crate using only libc, kept outside the repo
(`handover-proto`, about 400 lines). It ran on Linux 6.18.54.1 (WSL2), in a
private temp dir. Each experiment spawned and reaped its own processes, and the
temp dir was removed afterwards. It touched no tmux or rmux server.

**`pidfd`:** a process watches a grandchild it did not spawn.
```
[pidfd] pidfd_open(non-child) ok, fd=4
[pidfd] poll before exit: revents=0x0 (0 = still running)
[pidfd] waitid(P_PIDFD, WEXITED|WNOHANG) on running non-child: Err(ECHILD)
[pidfd] after pidfd_send_signal(SIGTERM): poll revents=0x1 after 127.599µs
[pidfd] waitid(P_PIDFD, WEXITED) on exited non-child: Err(ECHILD)
[holder] reaped grandchild: signal: 15 (SIGTERM)
```

**`subreaper`:**
```
[subreaper] holder killed; grandchild 863767 ppid now 863765   (= the subreaper)
[subreaper] waitid(P_PIDFD, WEXITED) after adoption: Ok((2, 15))   CLD_KILLED, SIGTERM
```

**`exec`, the re-exec in place.** A shell runs on a pty as session leader, as
rmux spawns it, with the master opened CLOEXEC as rmux does. A client connects
during the handover, and a line is written to the agent just before `execve`.
```
[exec] pid=864219 child=864220 pre-exec: Ok("got:before ppid=864219")
[exec] execve now
[exec] resumed: pid=864219 state="3 864220 4 864223 screen-blob-goes-here"
[exec] child output produced during exec, read after: Ok("got:during ppid=864219")
[exec] post-exec round trip: Ok("got:after ppid=864219")
[exec] accepted backlog client after exec: Ok("ping")
[exec] connector exited 0 (0 = got pong)
[exec] waitpid(child) after exec -> 864220, exited=true code=0
```
The pid, the parent, the pty master, the listener and its backlog all survived.
So did output produced while no one was reading. The state crossed in a memfd,
and the child was reaped by the new image.

**`rollback`.** The same sequence ran from a copy of the binary that had deleted
its own file. It execs into a "new" binary that refuses the state and
`execveat(AT_EMPTY_PATH)`s back into the deleted old image through a kept fd.
```
[rollback] new image pid=864329 cannot parse state; old exe = ".../old-proto (deleted)"
[exec] resumed: pid=864329 exe=".../old-proto (deleted)" ...
[exec] child output produced during exec, read after: Ok("got:during ppid=864329")
[exec] post-exec round trip: Ok("got:after ppid=864329")
[exec] waitpid(child) after exec -> 864330, exited=true code=0
```

**`scm`, the two-process handover.** An unrelated process receives the pty master
and a pidfd over SCM_RIGHTS, then the sender exits.
```
[scm] received master fd 4 and pidfd 5 for child 863780
[scm] old daemon exited: exit status: 0
[scm] child alive? pidfd poll revents=0x0; ppid now 379        (reparented to WSL Relay)
[scm] round trip through received master: Ok("got:hello ppid=379")
[scm] after SIGKILL via pidfd: poll revents=0x1 after 209.398µs
[scm] waitid(P_PIDFD) for the non-child: Err(ECHILD)
[scm] read(master) after child death -> -1 (EIO)
```
The pty works across SCM_RIGHTS, and the agent was not hung up when the sender
exited, because a session-leader child is not affected by its parent's exit. But
the exit status is gone.

**`rename`:** the two-process socket takeover.
```
[rename] after rename(new, path): new connect lands on new listener=true,
         old listener still has its earlier client=true
```

---

## 6. Effort and step plan

These estimates assume one engineer. Agent-assisted work runs faster, but the
testing is the long pole either way.

### rmux (about 3–4 weeks)

1. **Terminal-state DTOs and round trip**, 4–5 days.
   - DTOs and conversions for `Screen`, `Grid`, `GridLine`, `GridCell`, the
     hyperlinks, and the carried parser fields, with pending-bytes replay.
   - Property tests: feed random VT streams, then compare snapshot → restore →
     render with the original, including after further input.
   - Measure size and time at 50k history.
2. **Session-tree and store DTOs**, 4–6 days. `SessionStore` (including
   `LayoutTree`, which needs exposing), options, environment, hooks, key bindings,
   buffers, link groups, lifecycle, dead panes. Round-trip tests compare
   `list-*`/`show-*` output before and after.
3. **Runtime quiesce and resume**, 5–6 days.
   - Reader gates.
   - `PtyChild::Adopted` with waitpid reaping.
   - Disarming `PaneTerminal`/`SocketCleanup` drops.
   - Clearing CLOEXEC.
   - A `ServerDaemon::resume(fd)` entry beside `bind`.
   - A post-resume reap sweep.
   - Signal mask reset.
4. **Preamble, request, verify subprocess, `execveat`, rollback**, 3 days.
   - `SO_PEERCRED`/`SO_PEERPIDFD`.
   - The peer-exe fd.
   - `--handover-verify` and `--resume-fd` flags on both the rmux binary and the
     embedder API (`DaemonConfig::with_handover_argv`).
5. **Clients**, 1–2 days: the `[server upgrading]` stop reason and the control
   `%exit` reason. The `attach.reconnect` capability is phase 2, 2–3 days more.
6. **Wire-change support**, 1–2 days: the `listen` field, the grace listener, the
   `moved` marker.
7. **Integration tests**, 3–4 days.
   - Real ptys under continuous output across a handover, with no byte lost or
     duplicated.
   - An agent dying mid-exec.
   - A verify failure.
   - A forced post-exec failure leading to rollback.
   - A wire change between two builds with different `RMUX_WIRE_VERSION`
     (cfg-test override).
   - A busy-machine soak.

Release: cctop-rmux-server and -core take a minor bump (new public API), and
-client takes a minor for the stop reason.

### cctop (about 1 week)

1. **`cctop mux daemon --resume-fd N` and `--handover-verify FD`** in `mux::main`.
   `serve` gains `ServerDaemon::resume`. Half a day.
2. **The trigger** in `mux.rs`, 1–2 days.
   - `ensure_current()`: run `DaemonStatus`, compare against the linked
     cctop-rmux-server version, and request the handover.
   - Call it from `land_after_switch` / startup **before** `restore_running_tabs`
     (`crates/ui/src/switch.rs`, `panes.rs:954`), and from a new `cctop mux
     handover`.
   - The sibling-`w*` scan for wire changes.
   - Never block the UI: run on a thread, and show `⟳ upgrading agents' terminal
     server…` in the footer next to `UpdateCorner`.
3. **Tab reattach on `[server upgrading]`**, 1 day. An attach child that exits
   with that reason is re-run, not shown as ended.
4. **`cctop doctor`** (`src/doctor.rs` `mux_check`), 1 day.
   - Report the daemon's rmux version against the built-in one, its pid and
     uptime, any other `w*` daemons and their session counts, and a
     `handover.failed` file.
   - Offer `cctop mux handover` as the fix.
5. **Tests with `TestDaemon`**, 1–2 days: a handover with live panes keeps
   `cctop mux ls`, pane contents (`capture`), and the agent pids.

### Order

1. rmux 1–2 land first, as pure library work with round-trip tests and no
   behaviour change.
2. Then rmux 3–4 behind the request, which nothing sends yet.
3. Then cctop 1–2 and 4 together, with 3 alongside.
4. Wire-change support (rmux 6 and the cctop scan) can follow in a later
   release, but must land **before** the next `RMUX_WIRE_VERSION` bump.

## Risks

- **Serialisation completeness.** A field forgotten in a DTO is lost silently.
  Mitigations:
  - Exhaustive destructuring in the conversions (`let Screen { a, b, .. }` without
    `..`), so a new field fails to compile until it is handled.
  - Property round-trip tests.
- **Quiesce correctness.** A reader stopped at the wrong await point loses or
  duplicates bytes. The gate must sit only on `read_from_pane`, and the "no byte
  lost" integration test is the guard.
- **Residual double failure** (see the rollback table). Mitigated by verify, by
  rollback, and by CI round trips of the *previous* release's schema. Keep a
  fixture state file written by release N-1 in the repo.
- **Long daemon-wide pause.** All panes are frozen from quiesce to resume.
  Serialising 50k-line scrollbacks should take hundreds of ms, not seconds. The
  agents only see their writes block for that long, since the pty buffer is about
  4 KiB of line discipline plus pipe. A deadline aborts the handover, and resumes,
  if the snapshot runs long.
- **Unsupported features refuse the handover**, which is correct but can surprise.
  `doctor` must say why.
- **Bootstrapping.** Today's 0.10.x daemons have no preamble and need one last
  natural drain.
- **The kernel.** `pidfd_open` needs 5.3, and `SO_PEERPIDFD` 6.5 (optional, with a
  starttime fallback). `execveat` with `AT_EMPTY_PATH` and memfd are older still.
  The release ships static musl binaries, and `memfd_create`/`execveat` must go
  through `libc::syscall` or rustix, not glibc wrappers.
