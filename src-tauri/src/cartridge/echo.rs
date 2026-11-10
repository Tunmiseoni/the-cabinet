pub(crate) const UNIX: &str = "#!/bin/sh\nCABINET_ECHO_PROGRAM=1 CABINET_ECHO_ARGS=\"$*\" exec \"$CABINET_TEST_EXE\" --exact cartridge::echo::echo_program --nocapture --test-threads=1 -q\n";

pub(crate) const WINDOWS: &str = "@echo off\r\nset CABINET_ECHO_PROGRAM=1\r\nset \"CABINET_ECHO_ARGS=%*\"\r\n\"%CABINET_TEST_EXE%\" --exact cartridge::echo::echo_program --nocapture --test-threads=1 -q\r\nexit /b %ERRORLEVEL%\r\n";

pub(crate) fn install_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("CABINET_TEST_EXE", std::env::current_exe().unwrap());
    });
}

#[test]
fn echo_program() {
    if std::env::var("CABINET_ECHO_PROGRAM").as_deref() != Ok("1") {
        return;
    }
    let args: Vec<String> = std::env::var("CABINET_ECHO_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let stdin = std::io::stdin();
    let code = echo_cart::run(&args, stdin.lock(), std::io::stdout());
    std::process::exit(code);
}
