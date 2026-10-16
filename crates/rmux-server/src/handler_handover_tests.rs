//! The handover state of a realistic daemon: captured, encoded, decoded and
//! restored into a fresh daemon, which must then answer every query the
//! same way (docs/design/live-handover.md, phase 1).

use std::time::Instant;

use super::RequestHandler;
use crate::handover::{decode, encode, HandoverStateV1, PaneProcessV1, TerminalProfileV1};
use rmux_proto::{PaneTarget, SessionName};

/// Panes run `cat`, which writes nothing until spoken to, so the only output
/// in a transcript is what a test feeds it and two observations of one
/// daemon cannot race its programs.
const SCRIPT: &[&str] = &[
    "set-option -g history-limit 50000",
    "new-session -d -s alpha -x 120 -y 40 -n editor cat",
    "split-window -h -t alpha:0 cat",
    "split-window -v -t alpha:0.1 cat",
    "new-window -d -t alpha:3 -n logs cat",
    "select-layout -t alpha:0 main-vertical",
    "new-session -d -s beta -x 100 -y 30 cat",
    "split-window -v -t beta:0 cat",
    "resize-pane -Z -t beta:0.1",
    "new-session -d -s alpha-peer -t alpha",
    "rename-window -t alpha:3 renamed-logs",
    "set-option -g status-left 'handover #S'",
    "set-option -g @user-option 'a value with spaces'",
    "set-option -ga @list first",
    "set-option -w -t alpha:0 automatic-rename off",
    "set-option -w -t alpha:0 pane-border-style fg=red",
    "set-option -p -t alpha:0.1 @pane-option pane-value",
    "set-option -t beta status off",
    "set-option -g command-alias[100] 'hello=display-message hello'",
    "set-environment -g HANDOVER_GLOBAL 'global value'",
    "set-environment -t alpha HANDOVER_SESSION session-value",
    "set-environment -g -r HANDOVER_REMOVED",
    "set-environment -h -g HANDOVER_HIDDEN hidden",
    "set-hook -g after-new-window 'display-message created'",
    "set-hook -ga after-new-window 'display-message again'",
    "set-hook -t alpha session-renamed 'display-message renamed'",
    "bind-key -T root F12 display-message bound",
    "bind-key -N 'a note' -r -T prefix M-x { display-message one ; display-message two }",
    "unbind-key -T prefix c",
    "set-buffer -b named 'named buffer contents'",
    "set-buffer 'unnamed buffer'",
    "select-pane -m -t alpha:0.2",
    "select-pane -t alpha:0.1",
    "select-window -t alpha:3",
    "select-window -t alpha:0",
];

/// What each pane is fed: scrollback with styles and wide characters, an
/// alternate screen, a half-parsed sequence, and OSC 7501 records.
fn pane_output(scrollback_lines: u32) -> Vec<(&'static str, u32, u32, Vec<u8>)> {
    let mut scrollback = Vec::new();
    for line in 0..scrollback_lines {
        if line % 5 == 0 {
            scrollback.extend_from_slice(
                format!(
                    "\x1b[1;3{}m{line:05} 日本語 👍🏽 e\u{301}\x1b[0m \x1b]8;id=l{line};https://example.com/{line}\x1b\\link\x1b]8;;\x1b\\\r\n",
                    line % 8
                )
                .as_bytes(),
            );
        } else {
            scrollback.extend_from_slice(format!("{line:05} plain scrollback line\r\n").as_bytes());
        }
    }
    scrollback.extend_from_slice(b"\x1b]2;scrolled\x07\x1b]7;file://host/project\x07$ ");

    vec![
        ("alpha", 0, 0, scrollback),
        (
            "alpha",
            0,
            1,
            b"shell before vim\r\n\x1b[?1049h\x1b[2J\x1b[Hvim on the alternate screen\x1b[5;3H\x1b[?25l\x1b[>1u"
                .to_vec(),
        ),
        (
            "alpha",
            0,
            2,
            b"\x1b]7501;state=working:app=claude:title=VGhpbmtpbmc=\x07\x1b]7501;id=sub/1:state=blocked:kind=question:msg=UHJvY2VlZD8=\x1b\\working \x1b[3;5r\x1b(0lqk\x1b(B\x1b]2;half a ti"
                .to_vec(),
        ),
        ("alpha", 3, 0, "wide 中文字 at the edge\r\nnext\x1b[1;3".as_bytes().to_vec()),
        (
            "beta",
            0,
            0,
            b"\x1b]7501;state=done:app=codex:progress=100\x1b\\\x1b]10;rgb:ff/ee/dd\x07done\r\n".to_vec(),
        ),
        (
            "beta",
            0,
            1,
            // Ends on the first byte of a three-byte character.
            [
                "\u{1F468}\u{200D}\u{1F469} zero width joiners ".as_bytes(),
                &[0xe6],
            ]
            .concat(),
        ),
    ]
}

/// The queries a client, a script or cctop could make, whose answers must
/// survive a handover. Pane-level commands (`list-panes`, `capture-pane`,
/// pane formats) are not among them: they look the pane's process up, and
/// the processes come back in the resume runtime (`resume_from_handover`),
/// not here; `tests/live_handover.rs` compares those across a real exec. Panes
/// are compared through their transcripts instead ([`observe_panes`]).
const QUERIES: &[&str] = &[
    "list-sessions -F '#{session_name} #{session_id} #{session_windows} #{session_group} #{session_grouped} #{session_created} #{session_last_attached} #{session_activity}'",
    "list-windows -a -F '#{session_name}:#{window_index} #{window_id} #{window_name} #{window_layout} #{window_zoomed_flag} #{window_active} #{window_last_flag} #{window_flags} #{window_width}x#{window_height} #{window_panes}'",
    "show-options -s",
    "show-options -g",
    "show-options -gw",
    "show-options -t alpha",
    "show-options -t beta",
    "show-options -w -t alpha:0",
    "show-options -p -t alpha:0.1",
    "show-environment -g",
    "show-environment -t alpha",
    "show-environment -s -t alpha",
    "show-hooks -g",
    "show-hooks -t alpha",
    "list-keys",
    "list-keys -N",
    "list-buffers -F '#{buffer_name} #{buffer_size} #{buffer_created} #{buffer_sample}'",
    "show-buffer -b named",
    "show-buffer",
];

fn targets() -> Vec<&'static str> {
    vec![
        "alpha:0.0",
        "alpha:0.1",
        "alpha:0.2",
        "alpha:3.0",
        "beta:0.0",
        "beta:0.1",
    ]
}

async fn run(handler: &RequestHandler, command: &str) -> Result<String, String> {
    let parsed = handler
        .parse_control_commands(command)
        .await
        .map_err(|error| format!("parse: {error}"))?;
    let output = handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(output.stdout()).into_owned())
}

async fn feed(handler: &RequestHandler, session: &str, window: u32, pane: u32, bytes: &[u8]) {
    let target = PaneTarget::with_window(
        SessionName::new(session).expect("valid session name"),
        window,
        pane,
    );
    let transcript = handler
        .state
        .lock()
        .await
        .transcript_handle(&target)
        .expect("pane transcript exists");
    let _ = transcript
        .lock()
        .expect("pane transcript mutex must not be poisoned")
        .append_bytes(bytes);
}

/// The fixture uses a short scrollback so the checked-in files stay small;
/// the round trips use a long one.
async fn realistic_daemon(scrollback_lines: u32) -> RequestHandler {
    let handler = RequestHandler::new();
    for command in SCRIPT {
        if let Err(error) = run(&handler, command).await {
            panic!("{command}: {error}");
        }
    }
    for (session, window, pane, bytes) in pane_output(scrollback_lines) {
        feed(&handler, session, window, pane, &bytes).await;
    }
    handler
}

/// Every answer, keyed by its query, so a mismatch names the query.
async fn observe(handler: &RequestHandler) -> Vec<(String, Result<String, String>)> {
    let mut answers = Vec::new();
    for query in QUERIES {
        answers.push(((*query).to_owned(), run(handler, query).await));
    }
    for (target, answer) in observe_panes(handler).await {
        answers.push((target, Ok(answer)));
    }
    answers
}

/// What a pane shows, read from its transcript: the whole scrollback with
/// its styles and links, the alternate screen's saved grid, the cursor, the
/// modes, the title, the parser's unfinished sequence and the OSC 7501
/// records, as `capture-pane` and the pane formats would report them.
async fn observe_panes(handler: &RequestHandler) -> Vec<(String, String)> {
    use rmux_core::{GridRenderOptions, ScreenCaptureRange};

    let full = ScreenCaptureRange {
        start: None,
        end: None,
        start_is_absolute: true,
        end_is_absolute: true,
    };
    let styled = GridRenderOptions {
        with_sequences: true,
        ..GridRenderOptions::default()
    };
    let mut out = Vec::new();
    for target in targets() {
        let (session, rest) = target.split_once(':').expect("session:window.pane");
        let (window, pane) = rest.split_once('.').expect("window.pane");
        let pane_target = PaneTarget::with_window(
            SessionName::new(session).expect("valid session name"),
            window.parse().expect("window index"),
            pane.parse().expect("pane index"),
        );
        let transcript = handler
            .state
            .lock()
            .await
            .transcript_handle(&pane_target)
            .expect("pane transcript exists");
        let transcript = transcript
            .lock()
            .expect("pane transcript mutex must not be poisoned");
        let screen = transcript.screen();
        out.push((
            target.to_owned(),
            format!(
                "{}\n--- saved ---\n{:?}\n--- state ---\ncursor {:?} mode {:#x} alternate {} title {:?} path {:?} history {}/{} pending {:?} saved cursor {:?} program {:?}\n",
                String::from_utf8_lossy(&screen.capture_transcript(full, styled)),
                screen
                    .capture_saved_transcript(full, styled)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                screen.cursor_position(),
                transcript.mode(),
                transcript.is_alternate(),
                transcript.title(),
                screen.path(),
                transcript.history_size(),
                transcript.history_limit(),
                transcript.pending_bytes(),
                transcript.saved_cursor_state(),
                transcript.program_status_root(),
            ),
        ));
    }
    out
}

fn assert_same_answers(
    restored: &[(String, Result<String, String>)],
    original: &[(String, Result<String, String>)],
) {
    assert_eq!(restored.len(), original.len());
    for ((query, got), (_, want)) in restored.iter().zip(original) {
        assert_eq!(
            got, want,
            "`{query}` answers differently after the handover"
        );
    }
}

/// The restored daemon has no pane processes yet (the resume adopts them), and
/// fresh recency tokens; everything else must be what was captured.
fn without_runtime(mut state: HandoverStateV1) -> HandoverStateV1 {
    state.header.pane_fds.clear();
    for pane in &mut state.panes {
        pane.process = None;
    }
    let mut order: Vec<_> = state
        .sessions
        .sessions
        .iter()
        .map(|session| session.recency)
        .collect();
    order.sort_unstable();
    for session in &mut state.sessions.sessions {
        session.recency = order
            .iter()
            .position(|recency| *recency == session.recency)
            .expect("listed") as u64;
    }
    state
}

#[tokio::test]
async fn a_realistic_daemon_survives_capture_encode_decode_restore() {
    let original = realistic_daemon(3_000).await;
    let before = observe(&original).await;
    // The fixture exercises what it claims to: records, the alternate
    // screen and a half-parsed sequence are really there to be lost.
    let panes = format!("{:?}", observe_panes(&original).await);
    assert!(panes.contains("Working"), "{panes}");
    assert!(panes.contains("alternate true"), "{panes}");
    assert!(panes.contains("pending [27, 93"), "{panes}");
    let state = original
        .state
        .lock()
        .await
        .to_handover()
        .expect("a quiet daemon can be captured");
    assert_eq!(state.panes.len(), targets().len());
    assert!(
        state.panes.iter().all(|pane| pane.process.is_some()),
        "every live pane hands over its process"
    );
    assert_eq!(state.header.pane_fds.len(), targets().len());

    let bytes = encode(&state);
    let decoded = decode(&bytes).expect("this binary reads what it writes");
    assert_eq!(decoded, state);

    let restored = RequestHandler::new();
    let processes = restored
        .state
        .lock()
        .await
        .restore_handover(decoded)
        .expect("a captured state restores");

    // The processes come back for the resume runtime, each with its fd.
    let mut handed: Vec<_> = processes
        .iter()
        .map(|process| {
            (
                process.pane_id.as_u32(),
                process.master_fd,
                process.process.pid,
            )
        })
        .collect();
    handed.sort_unstable();
    let mut expected: Vec<_> = state
        .panes
        .iter()
        .map(|pane| {
            let fd = state
                .header
                .pane_fds
                .iter()
                .find(|(id, _)| *id == pane.pane_id)
                .expect("fd listed")
                .1;
            (
                pane.pane_id,
                fd,
                pane.process.as_ref().expect("process").pid,
            )
        })
        .collect();
    expected.sort_unstable();
    assert_eq!(handed, expected);

    assert_same_answers(&observe(&restored).await, &before);

    {
        let original = original.state.lock().await;
        let restored = restored.state.lock().await;
        assert_eq!(restored.options, original.options);
        assert_eq!(restored.environment, original.environment);
        assert_eq!(restored.hooks, original.hooks);
        assert_eq!(restored.key_bindings, original.key_bindings);
        assert_eq!(
            without_runtime(restored.to_handover().expect("restored state captures")),
            without_runtime(state)
        );
    }
}

#[tokio::test]
async fn a_restored_daemon_keeps_working() {
    let original = realistic_daemon(3_000).await;
    let state = original.state.lock().await.to_handover().expect("captures");
    let restored = RequestHandler::new();
    let _processes = restored
        .state
        .lock()
        .await
        .restore_handover(decode(&encode(&state)).expect("decodes"))
        .expect("restores");

    // The half-parsed title finishes on the restored parser.
    feed(&restored, "alpha", 0, 2, b"tle\x07").await;
    let panes = observe_panes(&restored).await;
    let (_, alpha_0_2) = panes
        .iter()
        .find(|(target, _)| target == "alpha:0.2")
        .expect("observed");
    assert!(alpha_0_2.contains("title \"half a title\""), "{alpha_0_2}");
    // New ids continue from the old counters instead of reusing them.
    run(&restored, "new-session -d -s gamma cat")
        .await
        .expect("a new session starts after the handover");
    let state = restored.state.lock().await.to_handover().expect("captures");
    let mut ids: Vec<_> = state
        .sessions
        .sessions
        .iter()
        .filter(|session| session.group_name.is_none() || session.name == "alpha")
        .flat_map(|session| session.windows.iter())
        .flat_map(|(_, window)| window.panes.iter().map(|pane| pane.id))
        .collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "pane ids stay unique: {ids:?}");
    drop(state);
    let windows = run(&restored, "list-windows -a -F '#{window_id}'")
        .await
        .expect("lists");
    let mut windows: Vec<_> = windows.lines().collect();
    windows.sort_unstable();
    windows.dedup();
    assert!(windows.len() >= 4, "{windows:?}");
}

/// What the verify subprocess checks, run in-process: a real daemon's state
/// passes, and each way a memfd can be wrong is refused with a reason.
#[tokio::test]
async fn the_verify_step_passes_a_real_state_and_names_what_is_wrong_with_a_bad_one() {
    use crate::handover::record::{append_record, ResumeRecordV1};
    use crate::handover::verify::verify_bytes;

    let original = realistic_daemon(200).await;
    let state = original.state.lock().await.to_handover().expect("captures");
    let record = ResumeRecordV1 {
        listener_fd: 9_999,
        socket_path: b"/tmp/rmux-verify/default".to_vec(),
        rollback_argv: vec![b"rmux".to_vec()],
    };
    let blob = |state: &HandoverStateV1, record: &ResumeRecordV1| {
        let mut bytes = encode(state);
        append_record(&mut bytes, record);
        bytes
    };

    let summary = verify_bytes(&blob(&state, &record)).expect("a real state verifies");
    assert!(summary.contains("6 processes"), "{summary}");

    let fd = state.header.pane_fds[0].1;
    let clash = ResumeRecordV1 {
        listener_fd: fd,
        ..record.clone()
    };
    let error = verify_bytes(&blob(&state, &clash)).expect_err("two owners of one fd");
    assert!(error.contains("not its own"), "{error}");

    let no_rollback = ResumeRecordV1 {
        rollback_argv: Vec::new(),
        ..record.clone()
    };
    assert!(verify_bytes(&blob(&state, &no_rollback)).is_err());

    let mut orphan = state.clone();
    orphan.header.pane_fds.clear();
    let error = verify_bytes(&blob(&orphan, &record)).expect_err("a process without an fd");
    assert!(error.contains("no fd"), "{error}");

    let whole = blob(&state, &record);
    assert!(
        verify_bytes(&whole[..whole.len() / 2]).is_err(),
        "truncated"
    );
    assert!(verify_bytes(&encode(&state)).is_err(), "no resume record");
}

#[tokio::test]
async fn a_handover_restores_only_into_an_empty_daemon() {
    let original = realistic_daemon(3_000).await;
    let state = original.state.lock().await.to_handover().expect("captures");
    let error = original
        .state
        .lock()
        .await
        .restore_handover(state)
        .expect_err("a live daemon refuses a second state");
    assert!(error.reason().contains("empty daemon"), "{error}");
}

// ---------------------------------------------------------------------------
// The checked-in fixture: what this release writes, which the next release
// must still read.
// ---------------------------------------------------------------------------

const FIXTURE: &[u8] = include_bytes!("handover/fixtures/schema-1.bin");
const FIXTURE_ANSWERS: &str = include_str!("handover/fixtures/schema-1.answers.txt");

/// The fixture is built from a real daemon, so everything that came from
/// this machine — the environment its panes started with, their directories
/// and shell — is replaced with stand-ins before it is written.
fn scrub(state: &mut HandoverStateV1) {
    let profile = TerminalProfileV1 {
        cwd: b"/fixture/project".to_vec(),
        shell: b"/bin/sh".to_vec(),
        environment: vec![
            (b"HOME".to_vec(), b"/fixture/home".to_vec()),
            (b"TERM".to_vec(), b"tmux-256color".to_vec()),
        ],
    };
    for (index, pane) in state.panes.iter_mut().enumerate() {
        pane.process = Some(PaneProcessV1 {
            pid: 40_000 + index as u32,
            exit_status: None,
            termination_attempted: false,
            runtime_window_name: pane
                .process
                .as_ref()
                .and_then(|process| process.runtime_window_name.clone()),
            profile: profile.clone(),
        });
    }
    for (index, (_, fd)) in state.header.pane_fds.iter_mut().enumerate() {
        *fd = 100 + index as i32;
    }
    for lifecycle in &mut state.pane_lifecycles {
        lifecycle.working_directory = Some(b"/fixture/project".to_vec());
        lifecycle.respawn_shell = b"/bin/sh".to_vec();
        lifecycle.private_environment.clear();
        lifecycle.respawn_environment.clear();
        if let crate::handover::LifecycleProcessV1::Running { pid } = &mut lifecycle.process {
            *pid = Some(40_000 + lifecycle.pane_id);
        }
    }
    for (_, environment) in &mut state.session_environments {
        *environment = profile.environment.clone();
    }
    for session in &mut state.sessions.sessions {
        session.cwd = Some(b"/fixture/project".to_vec());
    }
    // A pane's default title is the host name.
    for pane in &mut state.panes {
        let title = &mut pane.transcript.terminal.screen.title;
        if *title != "scrolled" {
            *title = "fixture-host".to_owned();
        }
    }
}

/// The fixture's answers are the queries whose output depends only on the
/// state, and not on what defaults a later release adds.
async fn fixture_answers(handler: &RequestHandler) -> String {
    let mut out = String::new();
    let queries: Vec<String> = vec![
        "list-sessions -F '#{session_name} #{session_id} #{session_windows} #{session_group}'".to_owned(),
        "list-windows -a -F '#{session_name}:#{window_index} #{window_id} #{window_name} #{window_layout} #{window_zoomed_flag} #{window_active}'".to_owned(),
        "show-options -gv @user-option".to_owned(),
        "show-options -gv status-left".to_owned(),
        "show-options -wv -t alpha:0 pane-border-style".to_owned(),
        "show-options -pv -t alpha:0.1 @pane-option".to_owned(),
        "show-environment -g HANDOVER_GLOBAL".to_owned(),
        "show-environment -t alpha HANDOVER_SESSION".to_owned(),
        "show-hooks -g after-new-window".to_owned(),
        "list-keys -T root".to_owned(),
        "list-buffers -F '#{buffer_name} #{buffer_size}'".to_owned(),
    ];
    for query in queries {
        out.push_str(&format!("$ {query}\n"));
        match run(handler, &query).await {
            Ok(answer) => out.push_str(&answer),
            Err(error) => out.push_str(&format!("error: {error}\n")),
        }
    }
    for (target, answer) in observe_panes(handler).await {
        out.push_str(&format!("$ pane {target}\n{answer}"));
    }
    out
}

/// Rewrites the fixture. Run it once when a release changes the schema, never
/// to make a failing fixture test pass:
/// `RMUX_WRITE_HANDOVER_FIXTURE=1 cargo test -p cctop-rmux-server write_handover_fixture -- --ignored`
#[tokio::test]
#[ignore = "writes the checked-in fixture"]
async fn write_handover_fixture() {
    if std::env::var_os("RMUX_WRITE_HANDOVER_FIXTURE").is_none() {
        return;
    }
    let original = realistic_daemon(120).await;
    let mut state = original.state.lock().await.to_handover().expect("captures");
    scrub(&mut state);
    let bytes = encode(&state);
    for private in [
        std::env::var("HOME").unwrap_or_default(),
        std::env::var("USER").unwrap_or_default(),
        std::env::var("PATH").unwrap_or_default(),
        std::fs::read_to_string("/etc/hostname")
            .unwrap_or_default()
            .trim()
            .to_owned(),
    ] {
        if private.len() > 2 {
            assert!(
                !bytes
                    .windows(private.len())
                    .any(|window| window == private.as_bytes()),
                "the fixture must not carry this machine's {private:?}"
            );
        }
    }
    let restored = RequestHandler::new();
    let _ = restored
        .state
        .lock()
        .await
        .restore_handover(decode(&bytes).expect("decodes"))
        .expect("restores");
    let answers = fixture_answers(&restored).await;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handover/fixtures");
    std::fs::create_dir_all(&dir).expect("fixture directory");
    std::fs::write(dir.join("schema-1.bin"), &bytes).expect("fixture written");
    std::fs::write(dir.join("schema-1.answers.txt"), answers).expect("answers written");
}

#[tokio::test]
async fn the_schema_1_fixture_still_restores_and_answers_the_same() {
    let state = decode(FIXTURE).expect("this release reads the schema-1 fixture");
    // Byte for byte: decoding loses nothing a re-encoding would put back.
    assert_eq!(encode(&state), FIXTURE);

    let restored = RequestHandler::new();
    let processes = restored
        .state
        .lock()
        .await
        .restore_handover(state)
        .expect("the fixture restores");
    assert_eq!(processes.len(), targets().len());
    assert_eq!(fixture_answers(&restored).await, FIXTURE_ANSWERS);
}

// ---------------------------------------------------------------------------
// The pause: how long capture, encode, decode and restore take for a daemon
// with about 50 000 lines of scrollback, and for the worst case of every pane
// full at cctop's `history-limit 50000`.
// ---------------------------------------------------------------------------

async fn timed_daemon(panes: u32, lines_per_pane: u32) -> RequestHandler {
    let handler = RequestHandler::new();
    run(&handler, "set-option -g history-limit 50000")
        .await
        .expect("sets");
    run(&handler, "new-session -d -s timed -x 200 -y 50 cat")
        .await
        .expect("starts");
    for _ in 1..panes {
        run(&handler, "split-window -t timed:0 cat")
            .await
            .expect("splits");
        run(&handler, "select-layout -t timed:0 tiled")
            .await
            .expect("tiles");
    }
    for pane in 0..panes {
        let mut bytes = Vec::new();
        for line in 0..lines_per_pane {
            // One line in four styled, as an agent's output with diffs and
            // highlights tends to be; the rest plain.
            if line % 4 == 0 {
                bytes.extend_from_slice(
                    format!("\x1b[32m+{line:06}\x1b[0m added \x1b[1mbold\x1b[0m 日本 text of a styled diff line\r\n").as_bytes(),
                );
            } else {
                bytes.extend_from_slice(
                    format!("{line:06} an ordinary line of agent output, about eighty columns wide....\r\n")
                        .as_bytes(),
                );
            }
        }
        feed(&handler, "timed", 0, pane, &bytes).await;
    }
    handler
}

async fn measure(label: &str, panes: u32, lines_per_pane: u32) {
    let original = timed_daemon(panes, lines_per_pane).await;
    let state_lock = original.state.lock().await;

    let started = Instant::now();
    let state = state_lock.to_handover().expect("captures");
    let captured = started.elapsed();
    let started = Instant::now();
    let bytes = encode(&state);
    let encoded = started.elapsed();
    drop(state_lock);

    let started = Instant::now();
    let decoded = decode(&bytes).expect("decodes");
    let decoded_in = started.elapsed();
    let restored = RequestHandler::new();
    let mut restored_state = restored.state.lock().await;
    let started = Instant::now();
    let _ = restored_state.restore_handover(decoded).expect("restores");
    let restored_in = started.elapsed();

    let history: usize = state
        .panes
        .iter()
        .map(|pane| pane.transcript.terminal.screen.grid.history.len())
        .sum();
    println!(
        "{label}: {panes} panes, {history} history lines, {:.1} MiB; \
         capture {captured:?}, encode {encoded:?}, decode {decoded_in:?}, restore {restored_in:?}; \
         total pause {:?}",
        bytes.len() as f64 / (1024.0 * 1024.0),
        captured + encoded + decoded_in + restored_in,
    );
}

/// `cargo test --release -p cctop-rmux-server handover_timing -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check; run in release"]
async fn handover_timing() {
    measure("50k lines across a few panes", 4, 12_500).await;
    measure("every pane full at history-limit 50000", 4, 50_000).await;
}
