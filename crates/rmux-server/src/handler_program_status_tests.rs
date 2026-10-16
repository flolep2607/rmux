//! OSC 7501 end to end: a real program in a real pane asks, is answered on
//! its pty input, reports, and the report reads back through pane formats.

use super::RequestHandler;
use rmux_proto::{
    DisplayMessageRequest, NewSessionRequest, OptionName, PaneTarget, Request, RespawnPaneRequest,
    Response, ScopeSelector, SessionName, SetOptionMode, SetOptionRequest, Target, TerminalSize,
};
use std::path::PathBuf;
use tokio::time::{sleep, Duration, Instant};

const PROGRAM_FORMAT: &str = "#{pane_program_state}|#{pane_program_kind}|\
     #{pane_program_progress}|#{pane_program_app}|#{pane_program_title}|#{pane_program_msg}";

fn unique_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rmux-program-status-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("temporary directory is created");
    dir
}

async fn display(handler: &RequestHandler, target: &PaneTarget, format: &str) -> String {
    let response = handler
        .handle(Request::DisplayMessage(DisplayMessageRequest {
            target: Some(Target::Pane(target.clone())),
            print: true,
            message: Some(format.to_owned()),
            empty_target_context: false,
        }))
        .await;
    let Response::DisplayMessage(response) = response else {
        panic!("expected display-message response, got {response:?}");
    };
    let output = response
        .command_output()
        .expect("display-message -p returns output");
    String::from_utf8(output.stdout().to_vec())
        .expect("format output is utf8")
        .trim_end_matches('\n')
        .to_owned()
}

async fn wait_for(handler: &RequestHandler, target: &PaneTarget, format: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = display(handler, target, format).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{format} never became {want:?}; last {got:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_program_is_answered_and_its_report_reads_back_until_it_exits() {
    let handler = RequestHandler::new();
    let alpha = SessionName::new("program-status").expect("valid session name");
    let target = PaneTarget::with_window(alpha.clone(), 0, 0);
    let dir = unique_temp_dir();
    let reply = dir.join("reply");
    let go = dir.join("go");

    assert!(matches!(
        handler
            .handle(Request::NewSession(NewSessionRequest {
                session_name: alpha.clone(),
                detached: true,
                size: Some(TerminalSize { cols: 80, rows: 24 }),
                environment: None,
            }))
            .await,
        Response::NewSession(_)
    ));
    assert!(matches!(
        handler
            .handle(Request::SetOption(SetOptionRequest {
                scope: ScopeSelector::Pane(target.clone()),
                option: OptionName::RemainOnExit,
                value: "on".to_owned(),
                mode: SetOptionMode::Replace,
            }))
            .await,
        Response::SetOption(_)
    ));

    // Raw mode, so the reply reaches `head` byte for byte and is not echoed
    // back into the pane's output. The report is sent only after the reply
    // has arrived, as a real program does; then the program waits for `go`
    // so the live state can be read before it exits. "UnVuIHRlc3RzPw" is
    // "Run tests?" in base64 without padding.
    let script = format!(
        "stty raw -echo; \
         printf '\\033]7501;?\\033\\\\'; \
         head -c 10 > '{reply}'; \
         printf '\\033]7501;state=blocked:kind=question:progress=40:app=test:msg=UnVuIHRlc3RzPw\\007'; \
         while [ ! -e '{go}' ]; do sleep 0.02; done",
        reply = reply.display(),
        go = go.display(),
    );
    assert!(matches!(
        handler
            .handle(Request::RespawnPane(Box::new(RespawnPaneRequest {
                target: target.clone(),
                kill: true,
                start_directory: None,
                environment: None,
                command: Some(vec![script]),
                process_command: None,
            })))
            .await,
        Response::RespawnPane(_)
    ));

    wait_for(
        &handler,
        &target,
        PROGRAM_FORMAT,
        "blocked|question|40|test||Run tests?",
    )
    .await;
    assert_eq!(
        std::fs::read(&reply).expect("the program saved the reply it read"),
        b"\x1b]7501;?\x1b\\",
        "the query is answered on the pane's input with the query's terminator"
    );

    // The program exits while blocked: nothing will resolve that record, so
    // the pane, kept by remain-on-exit, must stop showing it.
    std::fs::write(&go, b"").expect("go file is written");
    wait_for(&handler, &target, "#{pane_dead}", "1").await;
    wait_for(&handler, &target, PROGRAM_FORMAT, "|||||").await;

    let _ = std::fs::remove_dir_all(&dir);
}
