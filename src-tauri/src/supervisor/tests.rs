use super::*;
use std::path::PathBuf;
use std::sync::mpsc;

fn spec(program: &str, args: &[&str]) -> LaunchSpec {
    LaunchSpec {
        program: PathBuf::from(program),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        cwd: std::env::temp_dir(),
        envs: Vec::new(),
    }
}

fn capture_lines(dir: &std::path::Path) -> (Capture, mpsc::Receiver<String>) {
    let sink = logging::open_emulator_log(dir, "test").unwrap();
    let (sender, receiver) = mpsc::channel();
    let sender = Mutex::new(sender);
    let observer: LineObserver = Arc::new(move |line: &str| {
        let _ = sender.lock_or_recover().send(line.to_string());
    });
    (
        Capture {
            label: "test".to_string(),
            sink,
            observer: Some(observer),
            stdout: None,
        },
        receiver,
    )
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cabinet-supervisor-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn reports_the_exit_code() {
    #[cfg(unix)]
    let child = spawn(&spec("sh", &["-c", "exit 3"]), None).unwrap();
    #[cfg(windows)]
    let child = spawn(&spec("cmd", &["/C", "exit 3"]), None).unwrap();
    let exit = child.exit.wait_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(exit.code, Some(3));
    assert_eq!(child.exit.get(), Some(exit));
}

#[test]
fn exited_resolves_once_the_program_exits() {
    #[cfg(unix)]
    let child = spawn(&spec("sh", &["-c", "sleep 0.1; exit 4"]), None).unwrap();
    #[cfg(windows)]
    let child = spawn(&spec("cmd", &["/C", "exit 4"]), None).unwrap();
    let exit = tauri::async_runtime::block_on(child.exited());
    assert_eq!(exit.code, Some(4));
    let again = tauri::async_runtime::block_on(child.exited());
    assert_eq!(again, exit);
}

#[test]
fn fails_to_spawn_a_missing_program() {
    let err = spawn(&spec("cabinet-no-such-program", &[]), None)
        .err()
        .unwrap();
    assert!(err.to_string().contains("failed to launch"), "got {err}");
}

#[test]
fn captures_output_lines_into_the_sink_and_observer() {
    let dir = scratch_dir("capture");
    let (capture, lines) = capture_lines(&dir);
    #[cfg(unix)]
    let child = spawn(
        &spec("sh", &["-c", "echo hello; echo oops >&2"]),
        Some(capture),
    )
    .unwrap();
    #[cfg(windows)]
    let child = spawn(&spec("cmd", &["/C", "echo hello"]), Some(capture)).unwrap();
    child.exit.wait_timeout(Duration::from_secs(10)).unwrap();
    #[cfg(unix)]
    let expected = ["hello", "oops"];
    #[cfg(windows)]
    let expected = ["hello"];
    let mut seen: Vec<String> = expected
        .iter()
        .map(|_| {
            lines
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .trim()
                .to_string()
        })
        .collect();
    seen.sort();
    assert_eq!(seen, expected);
    let log = std::fs::read_to_string(logging::emulator_log_path(&dir, "test")).unwrap();
    for line in expected {
        assert!(log.contains(line), "got {log}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stop_kills_a_long_running_program() {
    #[cfg(unix)]
    let child = spawn(&spec("sleep", &["30"]), None).unwrap();
    #[cfg(windows)]
    let child = spawn(&spec("ping", &["-n", "30", "127.0.0.1"]), None).unwrap();
    assert!(child.exit.get().is_none());
    let started = Instant::now();
    assert!(child.stop_blocking(Duration::ZERO).is_some());
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn stdout_lines(dir: &std::path::Path) -> (Capture, mpsc::Receiver<String>) {
    let (mut capture, lines) = capture_lines(dir);
    capture.stdout = capture.observer.take();
    (capture, lines)
}

#[test]
fn piped_stdin_reaches_the_program() {
    let dir = scratch_dir("stdin");
    let (capture, lines) = stdout_lines(&dir);
    #[cfg(unix)]
    let child = spawn_with(&spec("cat", &[]), Some(capture), Stdin::Piped).unwrap();
    #[cfg(windows)]
    let child = spawn_with(&spec("findstr", &["^"]), Some(capture), Stdin::Piped).unwrap();
    assert!(child.send_line("hello"));
    assert!(child.send_line("world"));
    child.close_stdin();
    assert_eq!(
        lines.recv_timeout(Duration::from_secs(5)).unwrap().trim(),
        "hello"
    );
    assert_eq!(
        lines.recv_timeout(Duration::from_secs(5)).unwrap().trim(),
        "world"
    );
    assert!(!child.send_line("late"));
    let exit = child.exit.wait_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(exit.code, Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_inherited_stdin_takes_no_lines() {
    #[cfg(unix)]
    let child = spawn(&spec("sh", &["-c", "exit 0"]), None).unwrap();
    #[cfg(windows)]
    let child = spawn(&spec("cmd", &["/C", "exit 0"]), None).unwrap();
    assert!(!child.send_line("ignored"));
    child.exit.wait_timeout(Duration::from_secs(10)).unwrap();
}

#[test]
fn the_exit_is_reported_after_the_last_output_line() {
    for round in 0..20 {
        let dir = scratch_dir(&format!("drain-{round}"));
        let (capture, lines) = stdout_lines(&dir);
        #[cfg(unix)]
        let child = spawn(&spec("sh", &["-c", "echo first; echo last"]), Some(capture)).unwrap();
        #[cfg(windows)]
        let child = spawn(
            &spec("cmd", &["/C", "echo first& echo last"]),
            Some(capture),
        )
        .unwrap();
        tauri::async_runtime::block_on(child.exited());
        let seen: Vec<String> = lines
            .try_iter()
            .map(|line| line.trim().to_string())
            .collect();
        assert_eq!(seen, ["first", "last"], "round {round}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn async_stop_kills_a_program_that_ignores_the_farewell() {
    #[cfg(unix)]
    let child = spawn_with(&spec("sleep", &["30"]), None, Stdin::Piped).unwrap();
    #[cfg(windows)]
    let child = spawn_with(
        &spec("ping", &["-n", "30", "127.0.0.1"]),
        None,
        Stdin::Piped,
    )
    .unwrap();
    let started = Instant::now();
    let exit = tauri::async_runtime::block_on(
        child.stop(Duration::from_millis(200), Some("{\"type\":\"stop\"}")),
    );
    assert!(exit.is_some());
    assert!(started.elapsed() < Duration::from_secs(5));
    let again = tauri::async_runtime::block_on(child.stop(Duration::from_secs(5), None));
    assert_eq!(again, exit);
}

#[cfg(unix)]
mod unix_tree {
    use super::*;

    fn alive(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn gone_within(pid: libc::pid_t, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !alive(pid)
    }

    fn spawn_with_grandchild(script: &str, name: &str) -> (Supervised, libc::pid_t) {
        let dir = scratch_dir(name);
        let (capture, lines) = capture_lines(&dir);
        let child = spawn(&spec("sh", &["-c", script]), Some(capture)).unwrap();
        let grandchild: libc::pid_t = lines
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(grandchild));
        let _ = std::fs::remove_dir_all(dir);
        (child, grandchild)
    }

    #[test]
    fn stop_kills_the_whole_tree() {
        let (child, grandchild) = spawn_with_grandchild("sleep 30 & echo $!; wait", "tree-stop");
        child.stop_blocking(Duration::ZERO).unwrap();
        assert!(gone_within(grandchild, Duration::from_secs(5)));
    }

    #[test]
    fn a_leader_exit_kills_what_it_left_running() {
        let (child, grandchild) =
            spawn_with_grandchild("sleep 30 & echo $!; sleep 0.2", "tree-exit");
        let exit = child.exit.wait_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(exit.code, Some(0));
        assert!(gone_within(grandchild, Duration::from_secs(5)));
    }

    #[test]
    fn a_graceful_stop_lets_the_program_exit_on_its_own() {
        let dir = scratch_dir("grace");
        let (capture, lines) = capture_lines(&dir);
        let child = spawn(
            &spec(
                "sh",
                &[
                    "-c",
                    "trap 'exit 7' TERM; echo ready; while :; do sleep 0.05; done",
                ],
            ),
            Some(capture),
        )
        .unwrap();
        assert_eq!(
            lines.recv_timeout(Duration::from_secs(5)).unwrap().trim(),
            "ready"
        );
        let exit = child.stop_blocking(Duration::from_secs(5)).unwrap();
        assert_eq!(exit.code, Some(7));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn async_stop_lets_the_program_honour_the_farewell() {
        let child = spawn_with(
            &spec(
                "sh",
                &[
                    "-c",
                    "while read line; do [ \"$line\" = bye ] && exit 5; done",
                ],
            ),
            None,
            Stdin::Piped,
        )
        .unwrap();
        let started = Instant::now();
        let exit = tauri::async_runtime::block_on(child.stop(Duration::from_secs(5), Some("bye")));
        assert_eq!(exit.unwrap().code, Some(5));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn async_stop_terminates_before_it_kills() {
        let dir = scratch_dir("term");
        let (capture, lines) = capture_lines(&dir);
        let child = spawn_with(
            &spec(
                "sh",
                &[
                    "-c",
                    "trap 'exit 9' TERM; echo ready; while :; do sleep 0.05; done",
                ],
            ),
            Some(capture),
            Stdin::Piped,
        )
        .unwrap();
        assert_eq!(
            lines.recv_timeout(Duration::from_secs(5)).unwrap().trim(),
            "ready"
        );
        let exit =
            tauri::async_runtime::block_on(child.stop(Duration::from_millis(100), Some("stop")));
        assert_eq!(exit.unwrap().code, Some(9));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn async_stop_kills_a_program_that_ignores_sigterm() {
        let child = spawn_with(
            &spec("sh", &["-c", "trap '' TERM; while :; do sleep 0.05; done"]),
            None,
            Stdin::Piped,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        let exit = tauri::async_runtime::block_on(child.stop(Duration::from_millis(100), None));
        assert_eq!(exit.unwrap().code, None);
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
