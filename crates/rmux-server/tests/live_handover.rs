//! A live handover of a real daemon (docs/design/live-handover.md).
//!
//! The daemon is this test binary itself, run as an embedder would run
//! `ServerDaemon`: a re-exec in place needs a daemon process of its own, and
//! the test cannot exec. So `main` is two programs. With
//! `--live-handover-daemon` it is the daemon: it binds a private socket in a
//! temporary directory, says `ready <pid>` on stdout, and takes `handover`
//! lines on stdin, which it turns into `ServerHandle::handover`, the
//! in-process trigger. Without it, it is the test runner, which starts such
//! daemons, drives them through their socket with real commands and real
//! programs in their panes, and kills whatever it started when it is done,
//! failed or not.
//!
//! The handovers go into hard links of this binary, so `/proc/<pid>/exe`
//! shows which file the daemon is running: the new one after a handover, the
//! old one after a rollback, even when the old one's link was deleted, as an
//! update deletes it.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmux_proto::{
    encode_frame, FrameDecoder, Request, Response, SourceFileRequest,
    INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH,
};
use rmux_server::{DaemonConfig, HandoverOptions, HandoverResume, ServerDaemon};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DAEMON_FLAG: &str = "--live-handover-daemon";

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(code) = rmux_server::run_handover_verify_helper(args.iter().cloned()) {
        std::process::exit(code);
    }
    if args.first().is_some_and(|arg| arg == DAEMON_FLAG) {
        args.remove(0);
        std::process::exit(daemon_main(args));
    }
    std::process::exit(run_tests(&args));
}

// ---------------------------------------------------------------- the daemon

fn daemon_main(mut args: Vec<OsString>) -> i32 {
    let resume = match HandoverResume::take_from_args(&mut args) {
        Ok(resume) => resume,
        Err(error) => {
            eprintln!("daemon: {error}");
            return 2;
        }
    };
    let [flag, socket] = args.as_slice() else {
        eprintln!("daemon: expected --socket PATH, got {args:?}");
        return 2;
    };
    if flag != "--socket" {
        eprintln!("daemon: expected --socket PATH, got {args:?}");
        return 2;
    }
    let socket = PathBuf::from(socket);
    let program: OsString = std::env::args_os().next().expect("argv[0]");
    let mut config = DaemonConfig::new(socket.clone())
        .without_tmux_shim()
        .with_handover_command(vec![
            program,
            DAEMON_FLAG.into(),
            "--socket".into(),
            socket.into_os_string(),
        ]);
    if let Some(resume) = resume {
        config = config.resuming(resume);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("daemon runtime");
    runtime.block_on(async move {
        let server = match ServerDaemon::new(config).bind().await {
            Ok(server) => server,
            Err(error) => {
                eprintln!("daemon: {error}");
                return 1;
            }
        };
        say(&format!("ready {}", std::process::id()));
        let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
        // Unbuffered byte reads, so no line after the current one is ever
        // swallowed by a reader the exec then throws away.
        std::thread::spawn(move || {
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                match std::io::Read::read(&mut std::io::stdin().lock(), &mut byte) {
                    Ok(1) if byte[0] == b'\n' => {
                        let text = String::from_utf8_lossy(&line).into_owned();
                        line.clear();
                        if lines_tx.send(text).is_err() {
                            return;
                        }
                    }
                    Ok(1) => line.push(byte[0]),
                    _ => return,
                }
            }
        });
        while let Some(line) = lines.recv().await {
            let words: Vec<&str> = line.split(' ').collect();
            match words.as_slice() {
                ["handover", deadline_ms, executable, socket] => {
                    let mut options = HandoverOptions::new()
                        .with_deadline(Duration::from_millis(deadline_ms.parse().unwrap()));
                    if *executable != "-" {
                        let fd = std::fs::File::open(executable).expect("executable");
                        options = options.with_executable(fd.into());
                    }
                    if *socket != "-" {
                        let program: OsString = executable.into();
                        options = options.with_command(vec![
                            program,
                            DAEMON_FLAG.into(),
                            "--socket".into(),
                            (*socket).into(),
                        ]);
                    }
                    let error = server.handover(options).await;
                    say(&format!("error {:?} {}", error.kind(), error.reason()));
                }
                other => eprintln!("daemon: unknown command {other:?}"),
            }
        }
        // The runner closed stdin: it is done with this daemon.
        let _ = server.shutdown().await;
        0
    })
}

fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

// ---------------------------------------------------------------- the runner

type TestResult = Result<(), String>;
type Test = (&'static str, fn() -> TestResult);

const TESTS: &[Test] = &[
    (
        "a_handover_keeps_the_pid_the_children_every_byte_and_every_answer",
        a_handover_keeps_the_pid_the_children_every_byte_and_every_answer,
    ),
    (
        "a_resume_that_fails_rolls_back_into_the_deleted_old_binary",
        a_resume_that_fails_rolls_back_into_the_deleted_old_binary,
    ),
    (
        "an_active_pipe_or_background_job_refuses_the_handover",
        an_active_pipe_or_background_job_refuses_the_handover,
    ),
    (
        "a_handover_past_its_deadline_aborts_and_the_daemon_carries_on",
        a_handover_past_its_deadline_aborts_and_the_daemon_carries_on,
    ),
];

fn run_tests(args: &[OsString]) -> i32 {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in TESTS {
            println!("{name}: test");
        }
        return 0;
    }
    // libtest's flags are accepted and ignored; anything else filters.
    let ignored_only = args.iter().any(|arg| arg == "--ignored");
    let filters: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    let selected: Vec<_> = TESTS
        .iter()
        .filter(|_| !ignored_only)
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str())))
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in &selected {
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(test).unwrap_or_else(|panic| {
            Err(panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panicked".to_owned()))
        });
        match outcome {
            Ok(()) => println!("test {name} ... ok ({:.1?})", started.elapsed()),
            Err(error) => {
                println!("test {name} ... FAILED\n    {error}");
                failed.push(*name);
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len()
    );
    i32::from(!failed.is_empty()) * 101
}

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(format!($($message)+));
        }
    };
}

/// A temporary directory beside this binary, so hard links to it work, and
/// a daemon started from a link in it. Dropping it stops the daemon, kills
/// every pane process it had, and removes the directory.
struct Fixture {
    directory: PathBuf,
    socket: PathBuf,
    daemon: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    pane_pids: Vec<u32>,
}

impl Fixture {
    fn start(label: &str) -> Result<Self, String> {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let directory = base.join(format!("handover-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let old = directory.join("old-rmux");
        let new = directory.join("new-rmux");
        let me = std::env::current_exe().map_err(|error| error.to_string())?;
        for link in [&old, &new] {
            if std::fs::hard_link(&me, link).is_err() {
                std::fs::copy(&me, link).map_err(|error| error.to_string())?;
            }
        }
        let socket = directory.join("s");
        let mut daemon = Command::new(&old)
            .arg(DAEMON_FLAG)
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("SHELL", "/bin/sh")
            .spawn()
            .map_err(|error| error.to_string())?;
        let stdin = daemon.stdin.take();
        let stdout = daemon.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        let fixture = Self {
            directory,
            socket,
            daemon,
            stdin,
            lines,
            pane_pids: Vec::new(),
        };
        let ready = fixture.next_line(Duration::from_secs(30))?;
        ensure!(
            ready == format!("ready {}", fixture.daemon.id()),
            "the daemon said {ready:?}"
        );
        Ok(fixture)
    }

    fn pid(&self) -> u32 {
        self.daemon.id()
    }

    fn old_binary(&self) -> PathBuf {
        self.directory.join("old-rmux")
    }

    fn new_binary(&self) -> PathBuf {
        self.directory.join("new-rmux")
    }

    fn next_line(&self, timeout: Duration) -> Result<String, String> {
        self.lines
            .recv_timeout(timeout)
            .map_err(|error| format!("no line from the daemon within {timeout:?}: {error}"))
    }

    fn send(&mut self, line: &str) -> TestResult {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())
    }

    /// Asks for a handover; returns the daemon's next line, `ready <pid>` or
    /// `error <kind> <reason>`.
    fn handover(
        &mut self,
        deadline: Duration,
        executable: &Path,
        socket: Option<&Path>,
    ) -> Result<String, String> {
        self.send(&format!(
            "handover {} {} {}",
            deadline.as_millis(),
            executable.display(),
            socket.map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        ))?;
        self.next_line(Duration::from_secs(60))
    }

    fn run(&self, command: &str) -> Result<String, String> {
        run_command(&self.socket, command)
    }

    /// A pane command running `body` as a shell script, which keeps the
    /// programs out of the reach of command-line quoting.
    fn script(&self, name: &str, body: &str) -> Result<String, String> {
        let path = self.directory.join(format!("{name}.sh"));
        std::fs::write(&path, body).map_err(|error| error.to_string())?;
        Ok(format!("'sh {}'", path.display()))
    }

    /// Records every pane's pid, so a failed test still kills them.
    fn record_pane_pids(&mut self) -> Result<Vec<u32>, String> {
        let pids: Vec<u32> = self
            .run("list-panes -a -F '#{pane_pid}'")?
            .lines()
            .map(|pid| {
                pid.parse()
                    .map_err(|error| format!("pane pid {pid:?}: {error}"))
            })
            .collect::<Result<_, _>>()?;
        for pid in &pids {
            if !self.pane_pids.contains(pid) {
                self.pane_pids.push(*pid);
            }
        }
        Ok(pids)
    }

    fn exe(&self) -> String {
        std::fs::read_link(format!("/proc/{}/exe", self.pid()))
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Closing stdin asks the daemon to shut down, which ends its panes.
        drop(self.stdin.take());
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if matches!(self.daemon.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        for pid in &self.pane_pids {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Runs a command line on the daemon as a client does, and returns what it
/// printed.
fn run_command(socket: &Path, command: &str) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let request = Request::SourceFile(Box::new(SourceFileRequest {
            paths: vec![INTERNAL_CANONICAL_COMMAND_EXECUTION_PATH.to_owned()],
            quiet: false,
            parse_only: false,
            verbose: false,
            expand_paths: false,
            target: None,
            caller_cwd: None,
            stdin: Some(command.to_owned()),
        }));
        let mut stream = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::net::UnixStream::connect(socket),
        )
        .await
        .map_err(|_| format!("`{command}`: connect timed out"))?
        .map_err(|error| format!("`{command}`: connect: {error}"))?;
        let frame = encode_frame(&request).map_err(|error| error.to_string())?;
        stream
            .write_all(&frame)
            .await
            .map_err(|error| format!("`{command}`: write: {error}"))?;
        let mut decoder = FrameDecoder::new();
        let mut buffer = [0_u8; 65536];
        let response = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(response) = decoder
                    .next_frame::<Response>()
                    .map_err(|error| error.to_string())?
                {
                    return Ok::<_, String>(response);
                }
                let read = stream
                    .read(&mut buffer)
                    .await
                    .map_err(|error| error.to_string())?;
                if read == 0 {
                    return Err("the daemon closed the connection".to_owned());
                }
                decoder.push_bytes(&buffer[..read]);
            }
        })
        .await
        .map_err(|_| format!("`{command}`: no answer"))?
        .map_err(|error| format!("`{command}`: {error}"))?;
        match response {
            Response::Error(error) => Err(format!("`{command}`: {}", error.error)),
            response => Ok(response
                .command_output()
                .map(|output| String::from_utf8_lossy(output.stdout()).into_owned())
                .unwrap_or_default()),
        }
    })
}

/// A client that upgrades its connection (attach or control mode) and then
/// collects everything the daemon sends until it closes the connection.
///
/// Returns once the upgrade is answered; the thread's result is what the
/// client saw before the end.
fn upgraded_client(
    socket: &Path,
    request: Request,
) -> Result<std::thread::JoinHandle<Result<Vec<u8>, String>>, String> {
    let socket = socket.to_path_buf();
    let (upgraded, wait) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(async {
            let mut stream = tokio::net::UnixStream::connect(&socket)
                .await
                .map_err(|error| error.to_string())?;
            let frame = encode_frame(&request).map_err(|error| error.to_string())?;
            stream
                .write_all(&frame)
                .await
                .map_err(|error| error.to_string())?;
            let mut decoder = FrameDecoder::new();
            let mut buffer = [0_u8; 65536];
            loop {
                if let Some(response) = decoder
                    .next_frame::<Response>()
                    .map_err(|error| error.to_string())?
                {
                    if let Response::Error(error) = response {
                        let _ = upgraded.send(Err(error.error.to_string()));
                        return Err("upgrade refused".to_owned());
                    }
                    break;
                }
                let read = stream
                    .read(&mut buffer)
                    .await
                    .map_err(|error| error.to_string())?;
                if read == 0 {
                    return Err("closed before the upgrade".to_owned());
                }
                decoder.push_bytes(&buffer[..read]);
            }
            let _ = upgraded.send(Ok(()));
            let mut seen = decoder.remaining_bytes().to_vec();
            let until = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                match tokio::time::timeout_at(until, stream.read(&mut buffer)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => return Ok(seen),
                    Ok(Ok(read)) => seen.extend_from_slice(&buffer[..read]),
                    Err(_) => return Err("the daemon never closed the connection".to_owned()),
                }
            }
        })
    });
    match wait.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(error)) => Err(format!("upgrade refused: {error}")),
        Err(_) => Err("no upgrade answer".to_owned()),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn wait_for(
    what: &str,
    timeout: Duration,
    mut check: impl FnMut() -> Result<bool, String>,
) -> TestResult {
    let until = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        ensure!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The parent pid of a process, from `/proc`, or `None` if it is gone (or a
/// zombie, which is as good as gone for a pane).
fn live_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 2..];
    let mut fields = after_name.split(' ');
    let state = fields.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    fields.next()?.parse().ok()
}

/// The numbers of a counter pane's lines (`<tag> <n>`), in screen order.
fn counter_values(capture: &str, tag: &str) -> Vec<u64> {
    capture
        .lines()
        .filter_map(|line| {
            line.strip_prefix(tag)?
                .strip_prefix(' ')?
                .trim()
                .parse()
                .ok()
        })
        .collect()
}

/// Every number from 0 to the last, each once, in order: nothing the pane
/// wrote was lost or read twice.
fn check_contiguous(values: &[u64], tag: &str) -> TestResult {
    ensure!(!values.is_empty(), "the {tag} counter printed nothing");
    for (expected, value) in values.iter().enumerate() {
        ensure!(
            *value == expected as u64,
            "the {tag} counter skips or repeats at line {expected}: found {value} (around {:?})",
            &values[expected.saturating_sub(3)..(expected + 3).min(values.len())]
        );
    }
    Ok(())
}

/// The slow counter: a line every few milliseconds, all through the test.
const SLOW_COUNTER: &str = "i=0; while :; do echo \"slow $i\"; i=$((i+1)); sleep 0.002; done";
/// The fast counter: twenty thousand lines as fast as the pty takes them,
/// with short pauses so it is still writing when the handover comes.
const FAST_COUNTER: &str = "awk 'BEGIN { for (i = 0; i < 20000; i++) { print \"fast \" i; fflush(); if (i % 100 == 99) system(\"sleep 0.02\") } system(\"sleep 100000\") }'";
/// Stops half way through an SGR sequence until it is sent a line.
const MID_ESCAPE: &str =
    "stty -echo; printf 'plain \\033[1;3'; read x; printf '1mbold\\033[0m done'; sleep 100000";
const ALTERNATE: &str =
    "printf 'main screen\\r\\n\\033[?1049h\\033[2J\\033[Hon the alternate screen\\033[3;5H'; sleep 100000";
const PROGRAM_STATUS: &str =
    "printf '\\033]7501;state=working:app=handover-test:title=VGVzdA==\\007\\033]2;titled\\007status'; sleep 100000";

/// Every pane, as a client lists it. Nothing here moves while the counters
/// count: their cursors and foreground commands are left out.
const PANE_FORMAT: &str = "list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_pid} #{pane_width}x#{pane_height} #{pane_active} #{alternate_on} #{pane_title} [#{pane_program_state}|#{pane_program_app}|#{pane_program_title}]'";

/// Answers that must be the same before and after: everything but the
/// counters, which keep counting.
fn observe(fixture: &Fixture) -> Result<Vec<(String, String)>, String> {
    let mut answers = Vec::new();
    for query in [
        "list-sessions -F '#{session_name} #{session_id} #{session_windows} #{session_created}'",
        "list-windows -a -F '#{session_name}:#{window_index} #{window_id} #{window_name} #{window_layout} #{window_active}'",
        PANE_FORMAT,
        "show-options -g history-limit",
        "show-options -g @handover-marker",
        "capture-pane -p -e -t work:1.0",
        "capture-pane -p -e -t work:2.0",
        "capture-pane -p -e -a -t work:2.0",
        "capture-pane -p -e -t work:3.0",
        "display-message -p -t work:1.0 '#{cursor_x},#{cursor_y} #{cursor_flag}'",
        "display-message -p -t work:2.0 '#{cursor_x},#{cursor_y} #{cursor_flag} #{alternate_on}'",
        "display-message -p -t work:3.0 '#{pane_program_state} #{pane_program_title}'",
    ] {
        // An error is an answer too, and must be the same one after.
        let answer = fixture
            .run(query)
            .unwrap_or_else(|error| format!("error: {error}"));
        answers.push((query.to_owned(), answer));
    }
    Ok(answers)
}

fn a_handover_keeps_the_pid_the_children_every_byte_and_every_answer() -> TestResult {
    let mut fixture = Fixture::start("keeps")?;
    fixture.run("set-option -g history-limit 100000")?;
    fixture.run("set-option -g @handover-marker 'set before'")?;
    fixture.run(&format!(
        "new-session -d -s work -x 100 -y 30 {}",
        fixture.script("slow", SLOW_COUNTER)?
    ))?;
    fixture.run(&format!(
        "split-window -d -t work:0 {}",
        fixture.script("fast_counter", FAST_COUNTER)?
    ))?;
    fixture.run(&format!(
        "new-window -d -t work:1 {}",
        fixture.script("mid_escape", MID_ESCAPE)?
    ))?;
    fixture.run(&format!(
        "new-window -d -t work:2 {}",
        fixture.script("alternate", ALTERNATE)?
    ))?;
    fixture.run(&format!(
        "new-window -d -t work:3 {}",
        fixture.script("program_status", PROGRAM_STATUS)?
    ))?;
    let pids_before = fixture.record_pane_pids()?;
    ensure!(
        pids_before.len() == 5,
        "expected 5 panes, got {pids_before:?}"
    );

    wait_for("the static panes to draw", Duration::from_secs(20), || {
        Ok(fixture
            .run("capture-pane -p -t work:1.0")?
            .contains("plain")
            && fixture
                .run("display-message -p -t work:2.0 '#{alternate_on}'")?
                .trim()
                == "1"
            && fixture
                .run("display-message -p -t work:3.0 '#{pane_program_state}'")?
                .trim()
                == "working")
    })?;
    wait_for(
        "the fast counter to get going",
        Duration::from_secs(20),
        || {
            Ok(
                counter_values(&fixture.run("capture-pane -p -S - -t work:0.1")?, "fast").len()
                    > 2000,
            )
        },
    )?;
    let before = observe(&fixture)?;
    let daemon_pid = fixture.pid();

    // An attached client and a control client, which a handover stops with
    // the reason that tells them to come back.
    let attached = upgraded_client(
        &fixture.socket,
        Request::AttachSession(rmux_proto::AttachSessionRequest {
            target: rmux_proto::SessionName::new("work").map_err(|error| error.to_string())?,
        }),
    )?;
    let control = upgraded_client(
        &fixture.socket,
        Request::ControlMode(rmux_proto::ControlModeRequest {
            mode: rmux_proto::ControlMode::Plain,
            client_terminal: Default::default(),
            initial_command_count: 0,
        }),
    )?;

    let started = Instant::now();
    let new_binary = fixture.new_binary();
    let answer = fixture.handover(Duration::from_secs(10), &new_binary, None)?;
    ensure!(
        answer == format!("ready {daemon_pid}"),
        "the handover answered {answer:?}"
    );
    let pause = started.elapsed();
    eprintln!("    handover round trip, request to ready: {pause:.1?}");

    ensure!(
        fixture.exe() == new_binary.display().to_string(),
        "the daemon runs {:?}, not the new binary",
        fixture.exe()
    );
    let attached = attached
        .join()
        .map_err(|_| "the attached client panicked".to_owned())??;
    ensure!(
        contains(&attached, b"[server upgrading]\r\n"),
        "the attached client was not told the server is upgrading: {:?}",
        String::from_utf8_lossy(&attached[attached.len().saturating_sub(200)..])
    );
    let control = control
        .join()
        .map_err(|_| "the control client panicked".to_owned())??;
    ensure!(
        contains(&control, b"%exit server-upgrade\n"),
        "the control client was not told the server is upgrading: {:?}",
        String::from_utf8_lossy(&control[control.len().saturating_sub(200)..])
    );
    for pid in &pids_before {
        ensure!(
            live_parent(*pid) == Some(daemon_pid),
            "pane process {pid} is not a live child of the daemon after the handover"
        );
    }
    ensure!(
        fixture.run("display-message -p '#{pid}'")?.trim() == daemon_pid.to_string(),
        "the daemon reports another pid"
    );
    let after = observe(&fixture)?;
    for ((query, want), (_, got)) in before.iter().zip(&after) {
        ensure!(
            got == want,
            "`{query}` answers differently:\nbefore: {want:?}\nafter:  {got:?}"
        );
    }

    // Let both counters run on well past the handover, then read them whole.
    wait_for(
        "the fast counter to finish",
        Duration::from_secs(60),
        || {
            Ok(
                counter_values(&fixture.run("capture-pane -p -S - -t work:0.1")?, "fast").len()
                    >= 20000,
            )
        },
    )?;
    let slow_before =
        counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len();
    wait_for("the slow counter to go on", Duration::from_secs(20), || {
        Ok(
            counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len()
                > slow_before + 50,
        )
    })?;
    check_contiguous(
        &counter_values(&fixture.run("capture-pane -p -S - -t work:0.1")?, "fast"),
        "fast",
    )?;
    check_contiguous(
        &counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow"),
        "slow",
    )?;

    // The parser was left in the middle of `ESC [ 1 ; 3`; the program now
    // finishes the sequence, and the text after it must come out bold.
    fixture.run("send-keys -t work:1.0 Enter")?;
    wait_for(
        "the half-sent sequence to complete",
        Duration::from_secs(20),
        || {
            Ok(fixture
                .run("capture-pane -p -t work:1.0")?
                .contains("bold done"))
        },
    )?;
    let styled = fixture.run("capture-pane -p -e -t work:1.0")?;
    ensure!(
        styled.contains("\u{1b}[1;31mbold") || styled.contains("\u{1b}[1m\u{1b}[31mbold"),
        "the sequence cut by the handover did not resume: {styled:?}"
    );

    // New work after the handover.
    fixture.run("new-window -d -t work:4 'echo made after the handover; sleep 100000'")?;
    fixture.record_pane_pids()?;
    wait_for(
        "a pane made after the handover",
        Duration::from_secs(20),
        || {
            Ok(fixture
                .run("capture-pane -p -t work:4.0")?
                .contains("made after the handover"))
        },
    )?;
    fixture.run("set-option -g @handover-marker 'set after'")?;
    ensure!(
        fixture.run("show-options -gv @handover-marker")?.trim() == "set after",
        "options do not change after the handover"
    );
    Ok(())
}

fn a_resume_that_fails_rolls_back_into_the_deleted_old_binary() -> TestResult {
    let mut fixture = Fixture::start("rollback")?;
    fixture.run("set-option -g history-limit 100000")?;
    fixture.run(&format!(
        "new-session -d -s work -x 100 -y 30 {}",
        fixture.script("slow", SLOW_COUNTER)?
    ))?;
    fixture.run(&format!(
        "new-window -d -t work:1 {}",
        fixture.script("mid_escape", MID_ESCAPE)?
    ))?;
    let pids = fixture.record_pane_pids()?;
    wait_for("the panes to draw", Duration::from_secs(20), || {
        Ok(fixture
            .run("capture-pane -p -t work:1.0")?
            .contains("plain")
            && counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len() > 20)
    })?;
    let before = fixture.run(PANE_FORMAT)?;
    let daemon_pid = fixture.pid();

    // An update replaces the binary the daemon was started from.
    let old_binary = fixture.old_binary();
    std::fs::remove_file(&old_binary).map_err(|error| error.to_string())?;
    // The new binary verifies the state, then cannot resume it: the command
    // line it is given names another socket than the one it inherits.
    let wrong_socket = fixture.directory.join("not-the-socket");
    let new_binary = fixture.new_binary();
    let answer = fixture.handover(Duration::from_secs(10), &new_binary, Some(&wrong_socket))?;
    ensure!(
        answer == format!("ready {daemon_pid}"),
        "the rollback answered {answer:?}"
    );
    ensure!(
        fixture.exe() == format!("{} (deleted)", old_binary.display()),
        "the daemon runs {:?}, not the deleted old binary",
        fixture.exe()
    );
    let marker = std::fs::read_to_string(fixture.directory.join("handover.failed"))
        .map_err(|error| format!("no handover.failed marker: {error}"))?;
    ensure!(
        marker.contains("not-the-socket"),
        "the marker says {marker:?}"
    );
    for pid in &pids {
        ensure!(
            live_parent(*pid) == Some(daemon_pid),
            "pane process {pid} did not survive the rollback as the daemon's child"
        );
    }
    ensure!(
        fixture.run(PANE_FORMAT)? == before,
        "the panes changed across the rollback"
    );
    let slow_before =
        counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len();
    wait_for("the counter to go on", Duration::from_secs(20), || {
        Ok(
            counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len()
                > slow_before + 50,
        )
    })?;
    check_contiguous(
        &counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow"),
        "slow",
    )?;
    fixture.run("send-keys -t work:1.0 Enter")?;
    wait_for("the program to carry on", Duration::from_secs(20), || {
        Ok(fixture
            .run("capture-pane -p -t work:1.0")?
            .contains("bold done"))
    })?;
    fixture.run("new-window -d -t work:2 'echo after the rollback; sleep 100000'")?;
    fixture.record_pane_pids()?;
    wait_for(
        "a pane made after the rollback",
        Duration::from_secs(20),
        || {
            Ok(fixture
                .run("capture-pane -p -t work:2.0")?
                .contains("after the rollback"))
        },
    )?;
    Ok(())
}

fn an_active_pipe_or_background_job_refuses_the_handover() -> TestResult {
    let mut fixture = Fixture::start("refusal")?;
    fixture.run("new-session -d -s work -x 80 -y 24 'echo refusal pane; sleep 100000'")?;
    let pids = fixture.record_pane_pids()?;
    let daemon_pid = fixture.pid();
    let old_binary = fixture.old_binary();

    fixture.run("pipe-pane -t work:0.0 'cat > /dev/null'")?;
    let answer = fixture.handover(Duration::from_secs(10), &old_binary, None)?;
    ensure!(
        answer.starts_with("error Refused") && answer.contains("pipe-pane"),
        "a piped pane answered {answer:?}"
    );
    fixture.run("pipe-pane -t work:0.0")?;

    fixture.run("run-shell -b 'sleep 3'")?;
    let answer = fixture.handover(Duration::from_secs(10), &old_binary, None)?;
    ensure!(
        answer.starts_with("error Refused") && answer.contains("background job"),
        "a background job answered {answer:?}"
    );

    ensure!(fixture.pid() == daemon_pid, "the daemon was replaced");
    ensure!(
        fixture.exe() == old_binary.display().to_string(),
        "the daemon runs {:?} after a refusal",
        fixture.exe()
    );
    for pid in &pids {
        ensure!(
            live_parent(*pid) == Some(daemon_pid),
            "pane process {pid} did not survive a refusal"
        );
    }
    ensure!(
        fixture
            .run("capture-pane -p -t work:0.0")?
            .contains("refusal pane"),
        "the pane lost its screen"
    );
    fixture.run("new-window -d -t work:1 'echo still serving; sleep 100000'")?;
    fixture.record_pane_pids()?;
    wait_for(
        "a pane made after the refusals",
        Duration::from_secs(20),
        || {
            Ok(fixture
                .run("capture-pane -p -t work:1.0")?
                .contains("still serving"))
        },
    )?;
    Ok(())
}

fn a_handover_past_its_deadline_aborts_and_the_daemon_carries_on() -> TestResult {
    let mut fixture = Fixture::start("deadline")?;
    fixture.run("set-option -g history-limit 100000")?;
    fixture.run(&format!(
        "new-session -d -s work -x 100 -y 30 {}",
        fixture.script("slow", SLOW_COUNTER)?
    ))?;
    let pids = fixture.record_pane_pids()?;
    wait_for("the counter to start", Duration::from_secs(20), || {
        Ok(counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len() > 20)
    })?;
    let daemon_pid = fixture.pid();
    let old_binary = fixture.old_binary();

    // No snapshot and verify fit in a millisecond: whichever step is running
    // when it passes, the handover is abandoned and the readers let go.
    let answer = fixture.handover(Duration::from_millis(1), &fixture.new_binary(), None)?;
    ensure!(
        answer.starts_with("error DeadlineExceeded"),
        "a one-millisecond handover answered {answer:?}"
    );
    ensure!(
        fixture.exe() == old_binary.display().to_string(),
        "the daemon was replaced after an abort: {:?}",
        fixture.exe()
    );
    for pid in &pids {
        ensure!(
            live_parent(*pid) == Some(daemon_pid),
            "pane process {pid} did not survive the abort"
        );
    }
    let count = counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len();
    wait_for(
        "the counter to go on after the abort",
        Duration::from_secs(20),
        || {
            Ok(
                counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow").len()
                    > count + 50,
            )
        },
    )?;
    check_contiguous(
        &counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow"),
        "slow",
    )?;

    // And a handover with time to spare still works after the abort.
    let answer = fixture.handover(Duration::from_secs(10), &fixture.new_binary(), None)?;
    ensure!(
        answer == format!("ready {daemon_pid}"),
        "the handover after an abort answered {answer:?}"
    );
    check_contiguous(
        &counter_values(&fixture.run("capture-pane -p -S - -t work:0.0")?, "slow"),
        "slow",
    )?;
    Ok(())
}
