//! Round trips of the handover DTOs.
//!
//! The terminal is the part with real state machines in it, so most of this
//! is generated: varied VT streams, cut at an arbitrary byte so the parser is
//! often mid-sequence, captured, restored, and then fed the same
//! continuation. The repo has no property-testing crate, and these streams
//! are cheap to generate from a fixed seed, which also makes a failure
//! reproducible from its seed alone.

use rmux_proto::TerminalSize;

use super::{GridV1, HandoverError, TerminalStateV1};
use crate::input::OscColourSlot;
use crate::{GridRenderOptions, ScreenCaptureRange, TerminalScreen};

/// xorshift64*: enough randomness for fixtures, and no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

/// One fragment of terminal output. Between them they reach every part of
/// the state the DTOs carry: text of every width, SGR and colours, the
/// cursor and its saves, scroll regions, the alternate screen, tab stops,
/// titles and their stack, OSC 7, OSC 8, OSC 10-12, OSC 7501, Kitty
/// keyboard pushes, character sets, REP, erases and insertions.
fn fragment(rng: &mut Rng) -> String {
    const TEXT: &[&str] = &[
        "hello",
        "a longer line of plain text that wraps on narrow screens",
        "日本語テキスト",
        "e\u{301}",
        "👍🏽",
        "\t",
        "x",
        " trailing   ",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
    ];
    const CONTROL: &[&str] = &[
        "\r\n",
        "\r\n",
        "\n",
        "\r",
        "\x08",
        "\x1b[1;31m",
        "\x1b[0m",
        "\x1b[38;2;10;200;30;48;5;99m",
        "\x1b[4:3;58;5;196m",
        "\x1b[7;9;53m",
        "\x1b[H",
        "\x1b[5;10H",
        "\x1b[2J",
        "\x1b[K",
        "\x1b[1K",
        "\x1b[3L",
        "\x1b[2M",
        "\x1b[4@",
        "\x1b[3P",
        "\x1b[2X",
        "\x1b[3;8r",
        "\x1b[r",
        "\x1b[S",
        "\x1b[2T",
        "\x1b7",
        "\x1b8",
        "\x1b[s",
        "\x1b[u",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b[?47h",
        "\x1b[?47l",
        "\x1b[?1h",
        "\x1b[?25l",
        "\x1b[?25h",
        "\x1b[?2004h",
        "\x1b[?1000h\x1b[?1006h",
        "\x1b[?7l",
        "\x1b[?7h",
        "\x1b[?6h",
        "\x1b[?6l",
        "\x1b[4h",
        "\x1b[4l",
        "\x1b[3 q",
        "\x1bH",
        "\x1b[3g",
        "\x1b[0g",
        "\x1b]0;a title\x07",
        "\x1b]2;another title\x1b\\",
        "\x1b[22;0t",
        "\x1b[23;0t",
        "\x1b]7;file://host/home/user/project\x07",
        "\x1b]8;id=one;https://example.com/a\x1b\\link\x1b]8;;\x1b\\",
        "\x1b]8;;https://example.com/anon\x07anon\x1b]8;;\x07",
        "\x1b]10;rgb:ff/ee/dd\x07",
        "\x1b]11;#102030\x1b\\",
        "\x1b]110\x07",
        "\x1b]7501;state=working:app=claude:title=VGhpbmtpbmc=\x07",
        "\x1b]7501;state=blocked:kind=question:msg=UHJvY2VlZD8=\x1b\\",
        "\x1b]7501;id=sub/1:state=done:progress=100\x07",
        "\x1b[>1u",
        "\x1b[<u",
        "\x1b[=1;1u",
        "\x1b(0lqk\x1b(B",
        "\x1b)0\x0eqq\x0f",
        "z\x1b[4b",
        "\x1b[6n",
        "\x1b[c",
        "\x1bc",
        "\x1b[?1004h",
        "\x07",
    ];
    if rng.below(3) == 0 {
        rng.pick(TEXT).to_owned()
    } else {
        rng.pick(CONTROL).to_owned()
    }
}

fn stream(rng: &mut Rng, fragments: usize) -> Vec<u8> {
    let mut out = String::new();
    for _ in 0..fragments {
        out.push_str(&fragment(rng));
    }
    out.into_bytes()
}

/// Everything a client, a capture or a format could see of a terminal.
fn observe(terminal: &TerminalScreen) -> Vec<String> {
    let screen = terminal.screen();
    let full = ScreenCaptureRange {
        start: None,
        end: None,
        start_is_absolute: true,
        end_is_absolute: true,
    };
    let with_sequences = GridRenderOptions {
        with_sequences: true,
        include_empty_cells: true,
        trim_spaces: false,
        ..GridRenderOptions::default()
    };
    vec![
        format!(
            "{:?}",
            String::from_utf8_lossy(&screen.capture_transcript(full, with_sequences))
        ),
        format!(
            "{:?}",
            String::from_utf8_lossy(&screen.capture_transcript(full, GridRenderOptions::default()))
        ),
        format!(
            "{:?}",
            screen
                .capture_saved_transcript(full, with_sequences)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        ),
        format!("{:?}", screen.capture_line_format_flags(full)),
        format!("cursor {:?}", screen.cursor_position()),
        format!("mode {:#x} style {}", screen.mode(), screen.cursor_style()),
        format!("region {:?}", screen.scroll_region()),
        format!(
            "title {:?} stack {:?}",
            screen.title(),
            screen.title_stack()
        ),
        format!("path {:?}", screen.path()),
        format!("tabs {:?}", screen.tab_stops()),
        format!(
            "alternate {} saved {:?}",
            screen.is_alternate(),
            screen.alternate_saved_cursor()
        ),
        format!(
            "history {} {} limit {}",
            screen.history_size(),
            screen.history_bytes(),
            screen.history_limit()
        ),
        format!("sgr {:?}", terminal.active_cell_state_ansi()),
        format!("saved sgr {:?}", terminal.saved_cell_state_ansi()),
        format!("saved cursor {:?}", terminal.saved_cursor_state()),
        format!("pending {:?}", terminal.pending_bytes()),
        format!(
            "colours {:?} {:?} {:?}",
            terminal.dynamic_colour(OscColourSlot::Foreground),
            terminal.dynamic_colour(OscColourSlot::Background),
            terminal.dynamic_colour(OscColourSlot::Cursor)
        ),
        format!(
            "program status {:?}",
            terminal.program_status().iter().collect::<Vec<_>>()
        ),
        format!("ground timer {}", terminal.ground_timer_active()),
    ]
}

/// Line timestamps and the history stamp come from the clock, which two
/// terminals fed the same bytes a moment apart can read differently.
fn without_clock(mut state: TerminalStateV1) -> TerminalStateV1 {
    fn grid(grid: &mut GridV1) {
        grid.history_stamp = 0;
        for line in grid.history.iter_mut().chain(grid.visible.iter_mut()) {
            line.time = 0;
        }
    }
    grid(&mut state.screen.grid);
    if let Some(saved) = state.screen.saved_grid.as_mut() {
        grid(&mut saved.grid);
    }
    state
}

/// Captures `original`, restores it, and checks the copy is the original:
/// the same DTO back, the same observable state, and the same behaviour on
/// whatever `continuation` feeds both next.
fn assert_round_trip(original: &mut TerminalScreen, continuation: &[u8], context: &str) {
    let state = original.to_handover();
    let mut restored = TerminalScreen::from_handover(state.clone())
        .unwrap_or_else(|error| panic!("{context}: restore refused: {error}"));
    assert_eq!(restored.to_handover(), state, "{context}: DTO round trip");
    assert_eq!(observe(&restored), observe(original), "{context}: observed");

    original.feed(continuation);
    restored.feed(continuation);
    assert_eq!(
        restored.take_replies(),
        original.take_replies(),
        "{context}: replies after the continuation"
    );
    assert_eq!(
        observe(&restored),
        observe(original),
        "{context}: observed after the continuation"
    );
    assert_eq!(
        without_clock(restored.to_handover()),
        without_clock(original.to_handover()),
        "{context}: state after the continuation"
    );
}

#[test]
fn generated_streams_round_trip_at_any_cut() {
    for seed in 0..400_u64 {
        let mut rng = Rng::new(seed);
        let size = TerminalSize {
            cols: 1 + rng.below(100) as u16,
            rows: 1 + rng.below(40) as u16,
        };
        let history_limit = [0, 1, 7, 100, 2000][rng.below(5) as usize];
        let mut terminal = TerminalScreen::new(size, history_limit);
        if rng.below(4) == 0 {
            terminal.set_input_buffer_limit(32 + rng.below(64) as usize);
        }
        let fragments = 1 + rng.below(120) as usize;
        let bytes = stream(&mut rng, fragments);
        // Any byte boundary, so the cut lands inside escape sequences,
        // strings and UTF-8 as often as between them.
        let cut = rng.below(bytes.len() as u64 + 1) as usize;
        terminal.feed(&bytes[..cut]);
        // Replies would otherwise be compared after the continuation as
        // well; what matters there is that both answer the same new queries.
        let _ = terminal.take_replies();
        let mut continuation = bytes[cut..].to_vec();
        continuation.extend(stream(&mut rng, 20));
        assert_round_trip(
            &mut terminal,
            &continuation,
            &format!("seed {seed}, size {size:?}, cut {cut} of {}", bytes.len()),
        );
    }
}

#[test]
fn half_parsed_sequences_finish_after_restore() {
    let cases: &[(&str, &[u8], &[u8])] = &[
        ("osc title", b"\x1b]2;half-ti", b"tle\x07"),
        ("csi", b"\x1b[1;3", b"1mred"),
        ("bare esc", b"\x1b", b"[2Jcleared"),
        (
            "utf-8",
            "é".as_bytes().split_at(1).0,
            "é".as_bytes().split_at(1).1,
        ),
        (
            "wide utf-8",
            "日".as_bytes().split_at(2).0,
            "日".as_bytes().split_at(2).1,
        ),
        ("dcs", b"\x1bPtmux;\x1b\x1b]0;x", b"\x07\x1b\\after"),
        ("osc 7501", b"\x1b]7501;state=wor", b"king:app=a\x07"),
        (
            "osc 8",
            b"\x1b]8;id=x;https://ex",
            b"ample.com\x1b\\L\x1b]8;;\x1b\\",
        ),
    ];
    for (name, head, tail) in cases {
        let mut terminal = TerminalScreen::new(TerminalSize { cols: 20, rows: 4 }, 10);
        terminal.feed(b"before ");
        terminal.feed(head);
        assert!(
            !terminal.pending_bytes().is_empty(),
            "{name}: the head must leave the parser mid-sequence"
        );
        assert_round_trip(&mut terminal, tail, name);
    }
}

#[test]
fn rep_repeats_the_character_printed_before_the_handover() {
    let mut terminal = TerminalScreen::new(TerminalSize { cols: 10, rows: 2 }, 0);
    terminal.feed(b"a");
    let mut restored = TerminalScreen::from_handover(terminal.to_handover()).expect("restores");
    restored.feed(b"\x1b[3b");
    let line = restored.screen().capture_transcript(
        ScreenCaptureRange::new(Some(0), Some(0)),
        GridRenderOptions::default(),
    );
    assert_eq!(line, b"aaaa\n");
}

#[test]
fn a_large_scrollback_with_wide_characters_and_an_alternate_screen_round_trips() {
    let mut terminal = TerminalScreen::new(TerminalSize { cols: 80, rows: 24 }, 50_000);
    for line in 0..5_000 {
        if line % 7 == 0 {
            terminal.feed(
                format!("\x1b[1;3{}m{line} 日本語 👍🏽 styled\x1b[0m\r\n", line % 8).as_bytes(),
            );
        } else {
            terminal.feed(format!("{line} plain scrollback line\r\n").as_bytes());
        }
    }
    terminal.feed(b"\x1b]7501;state=working:app=claude:title=VGhpbmtpbmc=\x07");
    terminal.feed(b"\x1b[?1049h\x1b[Hvim on the alternate screen\x1b[5;5H");
    assert!(terminal.screen().is_alternate());
    assert!(
        terminal.program_status().root().is_some(),
        "the fixture must exercise program status"
    );
    assert_round_trip(&mut terminal, b"\x1b[?1049lback\r\n", "large scrollback");
}

fn refused(state: TerminalStateV1) -> HandoverError {
    match TerminalScreen::from_handover(state) {
        Ok(_) => panic!("a broken state must be refused"),
        Err(error) => error,
    }
}

#[test]
fn states_the_screen_code_could_not_use_are_refused() {
    let terminal = TerminalScreen::new(TerminalSize { cols: 10, rows: 3 }, 10);
    let good = terminal.to_handover();

    let mut state = good.clone();
    state.screen.grid.visible.pop();
    assert!(refused(state).reason().contains("visible rows"));

    let mut state = good.clone();
    state.screen.cursor_x = 10;
    assert!(refused(state).reason().contains("cursor"));

    let mut state = good.clone();
    state.screen.rlower = 3;
    assert!(refused(state).reason().contains("scroll region"));

    let mut state = good.clone();
    state.parser.osc_colours.pop();
    assert!(refused(state).reason().contains("OSC colours"));

    let mut state = good.clone();
    state
        .parser
        .program_status
        .entries
        .push(super::ProgramStatusEntryV1 {
            id: String::new(),
            state: "sleeping".to_owned(),
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
            updated: 1,
        });
    assert!(refused(state).reason().contains("program state"));

    let mut state = good;
    state.screen.grid.history.push(super::GridLineV1 {
        cells: Vec::new(),
        plain_text: Some("日本".to_owned()),
        width: 10,
        plain_text_data_end: 2,
        flags: 0,
        time: 0,
    });
    assert!(refused(state).reason().contains("non-ASCII"));
}

#[test]
fn session_tree_round_trips_and_keeps_recency_order() {
    use crate::SessionStore;
    use rmux_proto::SessionName;

    let name = |value: &str| SessionName::new(value).expect("valid name");
    let size = TerminalSize {
        cols: 120,
        rows: 40,
    };
    let mut store = SessionStore::new();
    store.create_session(name("alpha"), size).expect("alpha");
    store.create_session(name("beta"), size).expect("beta");
    store
        .create_grouped_session_with_base_index(name("alpha-peer"), size, 0, name("alpha"))
        .expect("grouped");
    store
        .session_mut(&name("beta"))
        .expect("beta exists")
        .split_active_pane()
        .expect("split");
    // An odd name that sanitising would change if it ran twice.
    store
        .create_session(name("back\\slash"), size)
        .expect("backslash");

    let state = store.to_handover();
    let restored = SessionStore::from_handover(state.clone()).expect("restores");
    let mut again = restored.to_handover();

    // Recency tokens are minted afresh; only their order is carried.
    let order = |state: &super::SessionStoreV1| {
        let mut sessions: Vec<_> = state
            .sessions
            .iter()
            .map(|session| (session.recency, session.name.clone()))
            .collect();
        sessions.sort();
        sessions
            .into_iter()
            .map(|(_, name)| name)
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&again), order(&state));
    let mut expected = state;
    for session in expected
        .sessions
        .iter_mut()
        .chain(again.sessions.iter_mut())
    {
        session.recency = 0;
    }
    assert_eq!(again, expected);
}
