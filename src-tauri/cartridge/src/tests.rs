use crate::manifest::{LateJoin, Scope, StepKind};
use crate::{Error, Manifest, Platform, Version};

pub(crate) const TEST_MANIFEST: &str = r#"
format      = 1
id          = "test"
version     = "0.1.0"
title       = "Test cartridge"
min_cabinet = "0.1.0"
platforms   = ["macos-aarch64", "linux-x86_64", "windows-x86_64"]

[room]
max_members = 4
late_join   = "always"

[programs.echo.run]
macos-aarch64  = "files/macos-aarch64/echo-cart"
linux-x86_64   = "files/linux-x86_64/echo-cart"
windows-x86_64 = "files/windows-x86_64/echo-cart.exe"

[lifecycle.session]
program = "echo"

[lifecycle.service]
program = "echo"
args    = ["--service"]

[capabilities.hosts]
names  = ["cabinet-test.invalid"]
target = "service"
scope  = "session"

[[touches]]
path = "${setting:scratch_dir}/touch-test.txt"

[[settings]]
key      = "scratch_dir"
label    = "Scratch folder"
type     = "dir"
required = true
"#;

const FULL_MANIFEST: &str = r#"
format      = 1
id          = "full"
version     = "1.0.0-rc.1"
title       = "Everything"
description = "Every section."
min_cabinet = "0.1.0"
platforms   = ["macos-aarch64", "windows-x86_64"]

[room]
late_join = "ask"

[[settings]]
key      = "game_dir"
label    = "Game folder"
type     = "dir"
required = true

[[settings]]
key    = "launcher"
label  = "Launcher"
type   = "file"
detect = "retroarch"

[[settings]]
key     = "mode"
label   = "Mode"
type    = "choice"
options = ["fast", "slow"]

[programs.controller]
args = ["--data", "${data}"]

[programs.controller.run]
macos-aarch64  = "files/macos-aarch64/ctl"
windows-x86_64 = "files/windows-x86_64/ctl.exe"

[programs.helper.run]
macos-aarch64  = "files/macos-aarch64/helper"
windows-x86_64 = "files/windows-x86_64/helper.exe"

[lifecycle.setup]
program = "controller"
args    = ["setup", "${asset:core}"]

[lifecycle.session]
program = "controller"

[lifecycle.service]
program = "helper"

[lifecycle.uninstall]
program = "controller"
args    = ["uninstall"]

[[assets]]
id = "core"
[assets.download.macos-aarch64]
url    = "https://example.com/core.dylib"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
[assets.download.windows-x86_64]
url    = "https://example.com/core.dll"
sha256 = "1111111111111111111111111111111111111111111111111111111111111111"

[[assets]]
id   = "ca-cert"
file = "files/ca/ca.pem"

[[checks]]
label   = "Game file"
path    = "${setting:game_dir}/game.bin"
sha256  = "2222222222222222222222222222222222222222222222222222222222222222"
message = "Your game file differs."

[capabilities]
low_ports = [80, 443]

[capabilities.hosts]
names  = ["api.example.test"]
target = "service"
scope  = "session"

[capabilities.trust_ca]
asset           = "ca-cert"
permitted_names = ["example.test"]
scope           = "install"
stores          = ["system", "bottle"]
bottle_setting  = "game_dir"

[capabilities.firewall]
scope   = "session"
inbound = [{ proto = "tcp", port = 55435 }]

[[touches]]
path = "${setting:game_dir}/config.ini"

[[touches]]
registry  = 'HKCU\Software\Example'
platforms = ["windows-x86_64"]
"#;

fn parse(text: &str) -> crate::Result<Manifest> {
    Manifest::parse(text, None)
}

fn problems(text: &str) -> Vec<String> {
    match parse(text) {
        Err(Error::Invalid(problems)) => problems,
        other => panic!("expected validation problems, got {other:?}"),
    }
}

fn assert_problem(text: &str, needle: &str) {
    let problems = problems(text);
    assert!(
        problems.iter().any(|problem| problem.contains(needle)),
        "no problem mentions {needle:?}: {problems:#?}"
    );
}

#[test]
fn parses_the_test_cartridge() {
    let manifest = parse(TEST_MANIFEST).unwrap();
    assert_eq!(manifest.id, "test");
    assert_eq!(manifest.version, Version::new(0, 1, 0));
    assert_eq!(manifest.room.max_members, 4);
    assert_eq!(manifest.room.late_join, LateJoin::Always);
    assert_eq!(
        manifest.program_path("echo", Platform::WindowsX86_64),
        Some("files/windows-x86_64/echo-cart.exe")
    );
    assert_eq!(manifest.program_path("echo", Platform::MacosX86_64), None);
    assert_eq!(
        manifest.lifecycle.step(StepKind::Service).unwrap().args,
        vec!["--service"]
    );
    assert!(manifest.lifecycle.step(StepKind::Setup).is_none());
    assert_eq!(
        manifest.capabilities.hosts.as_ref().unwrap().scope,
        Scope::Session
    );
    assert!(!manifest.capabilities.is_empty());
    assert_eq!(manifest.bundled_paths().len(), 3);
}

#[test]
fn parses_every_section() {
    let manifest = parse(FULL_MANIFEST).unwrap();
    assert!(manifest.version.is_prerelease());
    assert_eq!(manifest.room.max_members, 8);
    assert_eq!(manifest.room.late_join, LateJoin::Ask);
    assert_eq!(manifest.settings.len(), 3);
    assert_eq!(manifest.assets.len(), 2);
    assert_eq!(manifest.capabilities.low_ports, vec![80, 443]);
    assert!(manifest.touches[1].applies_to(Platform::WindowsX86_64));
    assert!(!manifest.touches[1].applies_to(Platform::MacosAarch64));
    assert!(manifest.touches[0].applies_to(Platform::MacosAarch64));
}

#[test]
fn refuses_an_unknown_format_before_anything_else() {
    let text = TEST_MANIFEST.replace("format      = 1", "format = 2\nfuture_field = true");
    assert!(matches!(parse(&text), Err(Error::UnsupportedFormat(2))));
    let missing = TEST_MANIFEST.replace("format      = 1", "");
    assert!(matches!(parse(&missing), Err(Error::Invalid(_))));
}

#[test]
fn refuses_a_cartridge_for_a_newer_cabinet_before_strict_parsing() {
    let text = TEST_MANIFEST.replace(
        "min_cabinet = \"0.1.0\"",
        "min_cabinet = \"9.0.0\"\nfuture_field = 1",
    );
    let current = Version::new(0, 1, 15);
    match Manifest::parse(&text, Some(&current)) {
        Err(Error::NeedsNewerCabinet { needed, current }) => {
            assert_eq!(needed, Version::new(9, 0, 0));
            assert_eq!(current, Version::new(0, 1, 15));
        }
        other => panic!("unexpected {other:?}"),
    }
    let older = TEST_MANIFEST.replace("min_cabinet = \"0.1.0\"", "min_cabinet = \"0.1.15\"");
    assert!(Manifest::parse(&older, Some(&current)).is_ok());
}

#[test]
fn rejects_unknown_fields() {
    let text = format!("{TEST_MANIFEST}\n[extra]\nkey = 1\n");
    assert!(matches!(parse(&text), Err(Error::Toml(_))));
    let typo = TEST_MANIFEST.replace("late_join   = \"always\"", "latejoin = \"always\"");
    assert!(matches!(parse(&typo), Err(Error::Toml(_))));
}

#[test]
fn rejects_bad_scalars_at_parse_time() {
    let platform = TEST_MANIFEST.replace("\"macos-aarch64\", \"linux", "\"macos-arm64\", \"linux");
    let err = parse(&platform).unwrap_err().to_string();
    assert!(err.contains("unknown platform"), "{err}");
    let version = TEST_MANIFEST.replace("version     = \"0.1.0\"", "version = \"0.1.0+b1\"");
    let err = parse(&version).unwrap_err().to_string();
    assert!(err.contains("one version is one build"), "{err}");
    let join = TEST_MANIFEST.replace("\"always\"", "\"sometimes\"");
    assert!(matches!(parse(&join), Err(Error::Toml(_))));
}

#[test]
fn validates_identity_and_platforms() {
    assert_problem(
        &TEST_MANIFEST.replace("id          = \"test\"", "id = \"Test Cart\""),
        "id \"Test Cart\"",
    );
    assert_problem(
        &TEST_MANIFEST.replace("id          = \"test\"", "id = \"../x\""),
        "id \"../x\"",
    );
    assert_problem(
        &TEST_MANIFEST.replace(
            "platforms   = [\"macos-aarch64\", \"linux-x86_64\", \"windows-x86_64\"]",
            "platforms = [\"macos-aarch64\", \"linux-x86_64\", \"windows-x86_64\", \"linux-x86_64\"]",
        ),
        "twice",
    );
    assert_problem(
        &TEST_MANIFEST.replace("max_members = 4", "max_members = 0"),
        "max_members",
    );
}

#[test]
fn validates_programs_and_lifecycle() {
    let missing_run =
        TEST_MANIFEST.replace("linux-x86_64   = \"files/linux-x86_64/echo-cart\"\n", "");
    assert_problem(&missing_run, "no `run` entry for linux-x86_64");
    let undeclared = TEST_MANIFEST.replace(
        "macos-aarch64  = \"files/macos-aarch64/echo-cart\"",
        "macos-aarch64  = \"files/macos-aarch64/echo-cart\"\nmacos-x86_64 = \"files/x/echo\"",
    );
    assert_problem(&undeclared, "macos-x86_64, which is not in `platforms`");
    for path in [
        "/bin/sh",
        "files/../../etc",
        "bin/echo",
        "files\\\\win.exe",
        "files//x",
    ] {
        let bad = TEST_MANIFEST.replace("files/linux-x86_64/echo-cart\"", &format!("{path}\""));
        assert_problem(&bad, "must be a relative path under files/");
    }
    let wrong_program = TEST_MANIFEST
        .replace("args    = [\"--service\"]", "")
        .replace(
            "[lifecycle.service]\nprogram = \"echo\"",
            "[lifecycle.service]\nprogram = \"standin\"",
        );
    assert_problem(&wrong_program, "program \"standin\" is not declared");
    let wasm = TEST_MANIFEST.replace(
        "[programs.echo.run]",
        "[programs.echo]\nkind = \"wasm\"\n[programs.echo.run]",
    );
    assert_problem(&wasm, "wasm");
    let no_session = TEST_MANIFEST.replace("[lifecycle.session]\nprogram = \"echo\"\n", "");
    assert!(matches!(parse(&no_session), Err(Error::Toml(_))));
}

#[test]
fn validates_variables() {
    let unknown = TEST_MANIFEST.replace("args    = [\"--service\"]", "args = [\"${HOME}\"]");
    assert_problem(&unknown, "unknown variable ${HOME}");
    let setting =
        TEST_MANIFEST.replace("args    = [\"--service\"]", "args = [\"${setting:nope}\"]");
    assert_problem(&setting, "setting \"nope\" is not declared");
    let asset = TEST_MANIFEST.replace("args    = [\"--service\"]", "args = [\"${asset:nope}\"]");
    assert_problem(&asset, "asset \"nope\" is not declared");
    let data_touch = TEST_MANIFEST.replace("${setting:scratch_dir}/touch-test.txt", "${data}/x");
    assert_problem(&data_touch, "${data} variables are not allowed here");
}

#[test]
fn validates_settings() {
    let twice = format!(
        "{TEST_MANIFEST}\n[[settings]]\nkey = \"scratch_dir\"\nlabel = \"Again\"\ntype = \"dir\"\n"
    );
    assert_problem(&twice, "declared twice");
    let choice =
        format!("{TEST_MANIFEST}\n[[settings]]\nkey = \"m\"\nlabel = \"M\"\ntype = \"choice\"\n");
    assert_problem(&choice, "a choice needs `options`");
    let options = format!(
        "{TEST_MANIFEST}\n[[settings]]\nkey = \"m\"\nlabel = \"M\"\ntype = \"bool\"\noptions = [\"a\"]\n"
    );
    assert_problem(&options, "only a choice takes `options`");
    let detector = format!(
        "{TEST_MANIFEST}\n[[settings]]\nkey = \"m\"\nlabel = \"M\"\ntype = \"string\"\ndetect = \"steam\"\n"
    );
    assert_problem(&detector, "unknown detector \"steam\"");
    assert_problem(&detector, "a detector needs a file or dir setting");
}

#[test]
fn validates_assets_and_checks() {
    let both = format!(
        "{TEST_MANIFEST}\n[[assets]]\nid = \"a\"\nfile = \"files/a\"\n[assets.download.macos-aarch64]\nurl = \"https://x.test/a\"\nsha256 = \"{}\"\n",
        "0".repeat(64)
    );
    assert_problem(&both, "exactly one of `file` or `download`");
    let partial = format!(
        "{TEST_MANIFEST}\n[[assets]]\nid = \"a\"\n[assets.download.macos-aarch64]\nurl = \"http://x.test/a\"\nsha256 = \"{}\"\n",
        "A".repeat(64)
    );
    assert_problem(&partial, "no download for linux-x86_64");
    assert_problem(&partial, "url must be https");
    assert_problem(&partial, "64 lowercase hex digits");
    let check = format!(
        "{TEST_MANIFEST}\n[[checks]]\nlabel = \"c\"\npath = \"${{platform}}/x\"\nsha256 = \"short\"\n"
    );
    assert_problem(&check, "${platform} variables are not allowed here");
    assert_problem(&check, "sha256 must be 64 lowercase hex digits");
}

#[test]
fn validates_capabilities() {
    let no_service = FULL_MANIFEST.replace("[lifecycle.service]\nprogram = \"helper\"\n", "");
    assert_problem(&no_service, "there is no lifecycle.service");
    assert_problem(&no_service, "low_ports needs a lifecycle.service");
    assert_problem(
        &FULL_MANIFEST.replace("[80, 443]", "[80, 8080, 80]"),
        "8080 is not a privileged port",
    );
    assert_problem(
        &FULL_MANIFEST.replace("[80, 443]", "[80, 80]"),
        "lists 80 twice",
    );
    for name in [
        "*.example.test",
        "localhost",
        "Example.test",
        "203.0.113.4",
        "-a.test",
        "a..test",
    ] {
        let hosts = FULL_MANIFEST.replace("\"api.example.test\"", &format!("\"{name}\""));
        assert_problem(&hosts, "is not a lowercase DNS name");
        let ca = FULL_MANIFEST.replace("[\"example.test\"]", &format!("[\"{name}\"]"));
        assert_problem(&ca, "is not a registrable lowercase DNS name");
    }
    for suffix in ["co.uk", "com.au", "github.io", "herokuapp.com"] {
        let ca = FULL_MANIFEST.replace("[\"example.test\"]", &format!("[\"{suffix}\"]"));
        assert_problem(&ca, "is a public suffix");
    }
    for name in ["example.co.uk", "example-game.net", "api.example-game.net"] {
        let ca = FULL_MANIFEST.replace("[\"example.test\"]", &format!("[\"{name}\"]"));
        Manifest::parse(&ca, None).unwrap();
    }
    let session_hosts = FULL_MANIFEST.replace(
        "target = \"service\"\nscope  = \"session\"",
        "target = \"service\"\nscope  = \"install\"",
    );
    assert_problem(&session_hosts, "hosts.scope must be \"session\"");
    let other_target = FULL_MANIFEST.replace("target = \"service\"", "target = \"203.0.113.9\"");
    assert!(matches!(parse(&other_target), Err(Error::Toml(_))));
    let downloaded_ca = FULL_MANIFEST.replace("asset           = \"ca-cert\"", "asset = \"core\"");
    assert_problem(&downloaded_ca, "must be bundled");
    let no_bottle = FULL_MANIFEST.replace("bottle_setting  = \"game_dir\"\n", "");
    assert_problem(&no_bottle, "the bottle store needs `bottle_setting`");
    let file_bottle = FULL_MANIFEST.replace(
        "bottle_setting  = \"game_dir\"",
        "bottle_setting = \"launcher\"",
    );
    assert_problem(&file_bottle, "setting \"launcher\" must be a dir");
    let stray = FULL_MANIFEST.replace(
        "stores          = [\"system\", \"bottle\"]",
        "stores = [\"system\"]",
    );
    assert_problem(&stray, "only used with the bottle store");
}

#[test]
fn validates_touches() {
    let registry_everywhere = FULL_MANIFEST.replace("platforms = [\"windows-x86_64\"]\n", "");
    assert_problem(&registry_everywhere, "limited to Windows");
    let hklm = FULL_MANIFEST.replace("HKCU\\Software\\Example", "HKLM\\Software\\Example");
    assert_problem(&hklm, "only HKCU keys");
    let both = format!("{TEST_MANIFEST}\n[[touches]]\npath = \"${{setting:scratch_dir}}/a\"\nregistry = 'HKCU\\x'\n");
    assert_problem(&both, "exactly one of `path` or `registry`");
}

#[test]
fn reports_every_problem_at_once() {
    let text = TEST_MANIFEST
        .replace("id          = \"test\"", "id = \"\"")
        .replace("title       = \"Test cartridge\"", "title = \" \"")
        .replace("max_members = 4", "max_members = 0");
    assert!(problems(&text).len() >= 3);
    let message = parse(&text).unwrap_err().to_string();
    assert!(
        message.starts_with("invalid cartridge.toml:\n- "),
        "{message}"
    );
}
