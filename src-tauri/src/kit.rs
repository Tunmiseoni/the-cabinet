use crate::cartridge::{catrust, privileged};
use crate::commands::helper;
use cabinet_cartridge::manifest::Manifest;
use cabinet_cartridge::platform::Platform;
use cabinet_helper::protocol::Change;
use std::io::{BufRead, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::time::{Duration, Instant};

const MANIFEST: &str = include_str!("../../cartridges/test/cartridge.toml");
const OWNER: &str = "friend-kit/test";
const PORT: u16 = 987;
const NAME: &str = "cabinet-test.invalid";
const OUTSIDE: &str = "other-test.invalid";
const BANNER: &str = "echo-cart";
const REPORT: &str = "report.txt";

#[cfg(windows)]
const ECHO: &str = "echo-cart.exe";
#[cfg(not(windows))]
const ECHO: &str = "echo-cart";

struct Report {
    path: PathBuf,
    failures: usize,
}

impl Report {
    fn open(path: PathBuf) -> Self {
        let _ = std::fs::write(&path, "");
        Self { path, failures: 0 }
    }

    fn line(&self, text: &str) {
        println!("{text}");
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&self.path) {
            let _ = writeln!(file, "{text}");
        }
    }

    fn heading(&self, text: &str) {
        self.line("");
        self.line(&format!("== {text}"));
    }

    fn note(&self, text: &str) {
        self.line(&format!("   note  {text}"));
    }

    fn check(&mut self, what: &str, passed: bool, detail: &str) -> bool {
        if !passed {
            self.failures += 1;
        }
        let mark = if passed { "ok   " } else { "FAIL " };
        if detail.is_empty() {
            self.line(&format!("   {mark} {what}"));
        } else {
            self.line(&format!("   {mark} {what} ({detail})"));
        }
        passed
    }

    fn outcome<T>(&mut self, what: &str, result: Result<T, String>) -> Option<T> {
        match result {
            Ok(value) => {
                self.check(what, true, "");
                Some(value)
            }
            Err(err) => {
                self.check(what, false, &err);
                None
            }
        }
    }
}

fn kit_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn pause(text: &str) {
    print!("\n>> {text} Press Enter to go on. ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().lines().next();
}

fn ask(question: &str) -> bool {
    loop {
        print!("\n>> {question} [y/n] ");
        let _ = std::io::stdout().flush();
        let answer = std::io::stdin()
            .lock()
            .lines()
            .next()
            .and_then(Result::ok)
            .unwrap_or_default();
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ => println!("   Please type y or n."),
        }
    }
}

fn answers(address: SocketAddr, wait: Duration) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(&address, wait).map_err(|err| {
        if err.kind() == std::io::ErrorKind::TimedOut {
            "no answer: the connection timed out".to_string()
        } else {
            format!("no answer: {err}")
        }
    })?;
    stream
        .set_read_timeout(Some(wait))
        .map_err(|err| err.to_string())?;
    let mut buffer = [0; 64];
    let read = stream
        .read(&mut buffer)
        .map_err(|err| format!("connected, but nothing answered: {err}"))?;
    if String::from_utf8_lossy(&buffer[..read]).starts_with(BANNER) {
        Ok(())
    } else {
        Err("connected, but something other than echo-cart answered".to_string())
    }
}

fn probe(address: &str) -> ExitCode {
    let target = match (address, PORT)
        .to_socket_addrs()
        .map(|mut found| found.next())
    {
        Ok(Some(target)) => target,
        _ => {
            println!("cannot find the address \"{address}\"");
            return ExitCode::from(2);
        }
    };
    match answers(target, Duration::from_secs(4)) {
        Ok(()) => {
            println!("ANSWERED: echo-cart on {target} is reachable");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("NO ANSWER from {target}: {err}");
            ExitCode::from(1)
        }
    }
}

fn now_secs() -> i64 {
    (crate::time::now_ms() / 1000) as i64
}

fn resolves_to_loopback() -> bool {
    (NAME, 0)
        .to_socket_addrs()
        .map(|mut found| found.any(|address| address.ip() == Ipv4Addr::LOCALHOST))
        .unwrap_or(false)
}

fn settles(wanted: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while resolves_to_loopback() != wanted && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    resolves_to_loopback() == wanted
}

fn hosts_path() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        cabinet_helper::system::hosts()
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from(cabinet_helper::system::HOSTS))
    }
}

fn tailnet_address() -> Option<String> {
    crate::net::provider(&crate::config::Config::default())
        .ok()?
        .self_address()
}

fn run_tool(program: &str, args: &[&str]) -> Result<String, String> {
    let output = crate::process::command(program)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run {program}: {err}"))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn firewall_state() -> String {
    #[cfg(target_os = "linux")]
    {
        let firewalld = ["/usr/bin/firewall-cmd", "/bin/firewall-cmd"]
            .into_iter()
            .find(|tool| Path::new(tool).exists())
            .map(|tool| run_tool(tool, &["--state"]).unwrap_or_default());
        let ufw = ["/usr/sbin/ufw", "/usr/bin/ufw", "/sbin/ufw"]
            .into_iter()
            .find(|tool| Path::new(tool).exists());
        let ufw_unit = run_tool("systemctl", &["is-active", "ufw"]).unwrap_or_default();
        format!(
            "firewalld: {}; ufw: {}",
            firewalld.map_or("not installed".to_string(), |state| state
                .trim()
                .to_string()),
            match ufw {
                Some(_) => format!("installed, service {}", ufw_unit.trim()),
                None => "not installed".to_string(),
            }
        )
    }
    #[cfg(target_os = "macos")]
    {
        run_tool(
            "/usr/libexec/ApplicationFirewall/socketfilterfw",
            &["--getglobalstate"],
        )
        .unwrap_or_else(|err| err)
        .trim()
        .to_string()
    }
    #[cfg(windows)]
    {
        run_tool(
            r"C:\Windows\System32\netsh.exe",
            &["advfirewall", "show", "allprofiles", "state"],
        )
        .unwrap_or_else(|err| err)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('-') && !line.starts_with("Ok"))
        .collect::<Vec<_>>()
        .join("; ")
    }
}

fn place_program(root: &Path, platform: Platform) -> Result<String, String> {
    let source = kit_dir().join(ECHO);
    let bytes =
        std::fs::read(&source).map_err(|err| format!("cannot read {}: {err}", source.display()))?;
    let digest = cabinet_cartridge::digest::sha256_bytes(&bytes);
    let dir = root
        .join("store")
        .join(digest)
        .join("files")
        .join(platform.as_str());
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let program = dir.join(ECHO);
    std::fs::write(&program, &bytes).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| err.to_string())?;
    }
    #[cfg(windows)]
    if let Ok(me) = cabinet_helper::win::current_user() {
        let _ = cabinet_helper::win::set_owner(&program, &me);
    }
    program
        .canonicalize()
        .ok()
        .as_deref()
        .and_then(cabinet_helper::firewall::path_text)
        .ok_or_else(|| "the program's path is not text".to_string())
}

fn entries() -> Vec<cabinet_helper::protocol::Entry> {
    helper::helper_status().entries
}

fn listen(program: &str) -> Result<Child, String> {
    let mut child = crate::process::command(program)
        .args(["listen", &PORT.to_string()])
        .spawn()
        .map_err(|err| format!("cannot start echo-cart: {err}"))?;
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if answers(local, Duration::from_secs(1)).is_ok() {
            return Ok(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("echo-cart stopped at once ({status})"));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("echo-cart does not answer on this machine".to_string());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn session(report: &mut Report, address: &str) {
    report.heading("Session: name redirect, low port and firewall opening");
    let Some(platform) = Platform::current() else {
        report.check("this system is supported", false, std::env::consts::OS);
        return;
    };
    let Some(home) = crate::env::home_dir() else {
        report.check("home folder found", false, "");
        return;
    };
    let root = home
        .join(cabinet_helper::system::APP_DATA)
        .join(format!("friend-kit-{}", std::process::id()));
    let Some(program) = report.outcome(
        "the test program is placed in a store-shaped folder",
        place_program(&root, platform),
    ) else {
        return;
    };
    let capabilities = match Manifest::parse(MANIFEST, None) {
        Ok(manifest) => manifest.capabilities,
        Err(err) => {
            report.check(
                "the test cartridge's manifest reads",
                false,
                &err.to_string(),
            );
            return;
        }
    };
    let changes = match privileged::session_changes(
        &capabilities,
        "127.0.0.1",
        std::slice::from_ref(&program),
        Some(&program),
    ) {
        Ok(changes) => changes,
        Err(err) => {
            report.check("the session's changes are worked out", false, &err);
            return;
        }
    };
    report.check(
        "nothing redirects the test name before the session",
        !resolves_to_loopback(),
        "",
    );
    let Some(helper) = privileged::system() else {
        report.check("the helper is available", false, "");
        return;
    };
    let Some(mut lease) = report.outcome("the helper takes a session", helper.lease()) else {
        return;
    };
    let mut applied = true;
    for change in &changes {
        let what = match change {
            Change::Hosts { .. } => "the helper redirects the test name",
            Change::Firewall { .. } => "the helper opens the firewall for port 987",
            Change::LowPorts { .. } => "the helper lets the program use port 987",
            Change::TrustCa { .. } => "the helper trusts a certificate",
        };
        applied &= report.outcome(what, lease.apply(OWNER, change)).is_some();
    }
    for entry in entries() {
        report.note(&format!("the helper holds: {:?}", entry.change));
    }
    #[cfg(target_os = "linux")]
    {
        use cabinet_helper::lowports::PortGrant;
        let granted = cabinet_helper::lowports::FileCapability.granted(&program);
        report.check(
            "the program carries the right to bind port 987",
            granted == Ok(true),
            &format!("{granted:?}"),
        );
    }
    #[cfg(windows)]
    let rules: Vec<String> = entries()
        .iter()
        .filter(|entry| matches!(entry.change, Change::Firewall { .. }))
        .map(|entry| cabinet_helper::firewall::rule_name(&entry.id))
        .collect();
    #[cfg(windows)]
    for rule in &rules {
        let exists = cabinet_helper::firewall::Netsh::exists(rule);
        report.check(
            &format!("Windows Firewall shows the rule \"{rule}\""),
            exists == Ok(true),
            &format!("{exists:?}"),
        );
    }
    report.check(
        "the test name points at this machine during the session",
        settles(true),
        "",
    );
    let listener = if applied {
        report.outcome(
            "echo-cart answers on port 987 on this machine",
            listen(&program),
        )
    } else {
        None
    };
    if let Some(mut listener) = listener {
        pause(&format!(
            "Now ask the other person to check this machine. They run:\n\n       friend-kit probe {address}\n\n   They should see ANSWERED. Wait until they tell you what they saw."
        ));
        let seen = ask("Did they see ANSWERED?");
        report.check(
            "the other machine reached port 987 during the session",
            seen,
            "",
        );

        report.outcome("the helper undoes the session", lease.release());
        report.check(
            "the helper holds nothing after the session",
            entries().is_empty(),
            "",
        );
        report.check(
            "echo-cart still answers on this machine",
            answers(
                SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
                Duration::from_secs(2),
            )
            .is_ok(),
            "",
        );
        pause(&format!(
            "The firewall opening is gone, but echo-cart is still running. Ask the other person to check again:\n\n       friend-kit probe {address}\n\n   This time they should see NO ANSWER."
        ));
        let blocked = !ask("Did they see ANSWERED this time?");
        report.check(
            "the other machine could not reach port 987 after the session",
            blocked,
            "if no firewall is switched on here, this is expected to fail",
        );
        let _ = listener.kill();
        let _ = listener.wait();
    } else {
        report.outcome("the helper undoes the session", lease.release());
    }
    report.check("the test name is no longer redirected", settles(false), "");
    #[cfg(target_os = "linux")]
    {
        use cabinet_helper::lowports::PortGrant;
        let granted = cabinet_helper::lowports::FileCapability.granted(&program);
        report.check(
            "the program no longer carries the low-port right",
            granted == Ok(false),
            &format!("{granted:?}"),
        );
    }
    #[cfg(windows)]
    for rule in &rules {
        let exists = cabinet_helper::firewall::Netsh::exists(rule);
        report.check(
            &format!("the rule \"{rule}\" is gone"),
            exists == Ok(false),
            &format!("{exists:?}"),
        );
    }
    #[cfg(windows)]
    {
        let all = run_tool(
            r"C:\Windows\System32\netsh.exe",
            &[
                "advfirewall",
                "firewall",
                "show",
                "rule",
                "name=all",
                "verbose",
            ],
        )
        .unwrap_or_default();
        let marker = root.to_string_lossy().to_ascii_lowercase();
        report.check(
            "Windows added no rules of its own for echo-cart",
            !all.to_ascii_lowercase().contains(&marker),
            "if Windows asked whether to allow echo-cart, it may have added some",
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn read_kit_file(name: &str) -> Result<Vec<u8>, String> {
    let path = kit_dir().join(name);
    std::fs::read(&path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}

#[cfg(target_os = "linux")]
fn verifies(leaf: &Path, name: &str) -> bool {
    crate::process::command("/usr/bin/openssl")
        .args(["verify", "-purpose", "sslserver", "-verify_hostname", name])
        .arg(leaf)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "macos")]
fn verifies(leaf: &Path, name: &str) -> bool {
    crate::process::command("/usr/bin/security")
        .args(["verify-cert", "-L", "-p", "ssl", "-n", name, "-c"])
        .arg(leaf)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(windows)]
fn verifies(leaf: &Path, name: &str) -> bool {
    use windows_sys::Win32::Security::Cryptography::*;
    let Ok(pem) = std::fs::read(leaf) else {
        return false;
    };
    let Ok(der) = cabinet_cartridge::ca::der(&pem) else {
        return false;
    };
    unsafe {
        let leaf = CertCreateCertificateContext(X509_ASN_ENCODING, der.as_ptr(), der.len() as u32);
        if leaf.is_null() {
            return false;
        }
        let para = CERT_CHAIN_PARA {
            cbSize: std::mem::size_of::<CERT_CHAIN_PARA>() as u32,
            ..std::mem::zeroed()
        };
        let mut chain = std::ptr::null_mut();
        let built = CertGetCertificateChain(
            std::ptr::null_mut(),
            leaf,
            std::ptr::null(),
            std::ptr::null_mut(),
            &para,
            0,
            std::ptr::null(),
            &mut chain,
        );
        if built == 0 {
            CertFreeCertificateContext(leaf);
            return false;
        }
        let mut server = cabinet_helper::win::wide(name);
        let mut https = HTTPSPolicyCallbackData {
            Anonymous: HTTPSPolicyCallbackData_0 {
                cbStruct: std::mem::size_of::<HTTPSPolicyCallbackData>() as u32,
            },
            dwAuthType: AUTHTYPE_SERVER,
            fdwChecks: 0,
            pwszServerName: server.as_mut_ptr(),
        };
        let policy = CERT_CHAIN_POLICY_PARA {
            cbSize: std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32,
            dwFlags: 0,
            pvExtraPolicyPara: (&mut https as *mut HTTPSPolicyCallbackData).cast(),
        };
        let mut status = CERT_CHAIN_POLICY_STATUS {
            cbSize: std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32,
            ..std::mem::zeroed()
        };
        let checked =
            CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain, &policy, &mut status);
        let errors = (*chain).TrustStatus.dwErrorStatus;
        CertFreeCertificateChain(chain);
        CertFreeCertificateContext(leaf);
        checked != 0 && status.dwError == 0 && errors == CERT_TRUST_NO_ERROR
    }
}

fn certificate(report: &mut Report) {
    report.heading("Certificate: a throwaway authority limited to cabinet-test.invalid");
    let Some(pem) = report.outcome(
        "the throwaway certificate is in the kit",
        read_kit_file("ca.pem"),
    ) else {
        return;
    };
    let Some(ca) = report.outcome(
        "the certificate passes The Cabinet's checks",
        cabinet_cartridge::ca::check(&pem, &[NAME.to_string()], now_secs()),
    ) else {
        return;
    };
    let inside = kit_dir().join("inside.pem");
    let outside = kit_dir().join("outside.pem");
    report.check("nothing trusts it before", !verifies(&inside, NAME), "");
    let Some(store) = catrust::system() else {
        report.check("this system has a certificate store", false, "");
        return;
    };
    #[cfg(windows)]
    println!("\n   Windows will now ask whether to install a certificate from \"Cabinet throwaway test CA\". Click Yes.");
    #[cfg(target_os = "macos")]
    println!(
        "\n   macOS will now ask for your password to change your certificate trust settings."
    );
    if report
        .outcome("the certificate is trusted", store.trust(&ca))
        .is_some()
    {
        report.check(
            "the store holds it",
            store.holds(&ca.sha256) == Ok(true),
            "",
        );
        #[cfg(target_os = "linux")]
        for entry in entries() {
            if let Change::TrustCa { anchor, .. } = entry.change {
                report.note(&format!("placed at {anchor}"));
            }
        }
        report.check(
            "a certificate for cabinet-test.invalid is accepted",
            verifies(&inside, NAME),
            "",
        );
        report.check(
            "a certificate for another name is refused",
            !verifies(&outside, OUTSIDE),
            "",
        );
        report.check(
            "the test certificate is refused for another name",
            !verifies(&inside, OUTSIDE),
            "",
        );
    }
    #[cfg(windows)]
    println!("\n   Windows will now ask whether to delete that certificate. Click Yes.");
    report.outcome(
        "the certificate is removed again",
        store.untrust(&ca.sha256, &ca.der),
    );
    report.check(
        "the store no longer holds it",
        store.holds(&ca.sha256) == Ok(false),
        "",
    );
    report.check(
        "cabinet-test.invalid is no longer accepted",
        !verifies(&inside, NAME),
        "",
    );
}

fn leftovers() -> Vec<String> {
    #[cfg(target_os = "linux")]
    let paths: Vec<PathBuf> = [
        cabinet_helper::system::UNIT,
        cabinet_helper::system::INSTALLED,
        cabinet_helper::system::STATE_DIR,
        cabinet_helper::system::SOCKET,
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    #[cfg(target_os = "macos")]
    let paths: Vec<PathBuf> = vec![PathBuf::from(cabinet_helper::system::INSTALLED)];
    #[cfg(windows)]
    let paths: Vec<PathBuf> = cabinet_helper::system::state_dir().into_iter().collect();
    let found: Vec<String> = paths
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect();
    #[cfg(windows)]
    let found = {
        let mut found = found;
        if cabinet_helper::system::is_installed() {
            found.push("the com.the-cabinet.helper service".to_string());
        }
        let rules = run_tool(
            r"C:\Windows\System32\netsh.exe",
            &["advfirewall", "firewall", "show", "rule", "name=all"],
        )
        .unwrap_or_default();
        if rules.contains("The Cabinet ") {
            found.push("a Windows Firewall rule named The Cabinet ...".to_string());
        }
        found
    };
    found
}

fn guided() -> ExitCode {
    let mut report = Report::open(kit_dir().join(REPORT));
    println!(
        "The Cabinet friend kit\n\nThis checks The Cabinet's helper on this machine and leaves nothing behind. It will:\n  1. install the helper (your system asks for your password or approval),\n  2. run a test session while the other person checks they can reach this machine,\n  3. trust and remove a throwaway certificate,\n  4. remove the helper again (a second password or approval).\nIt writes everything to {} next to this program; send that file back afterwards.",
        REPORT
    );
    if !ask("Go ahead?") {
        return ExitCode::SUCCESS;
    }
    report.heading("This machine");
    report.note(&format!(
        "kit {} on {} {}",
        option_env!("FRIEND_KIT_BUILD").unwrap_or("local"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    report.note(&format!("firewall: {}", firewall_state()));
    let address = tailnet_address();
    report.check(
        "Tailscale gives this machine a tailnet address",
        address.is_some(),
        "",
    );
    let address = address.unwrap_or_else(|| "<this machine's tailnet address>".to_string());
    println!("\n   This machine's tailnet address is {address}. Tell the other person.");
    let hosts = hosts_path().and_then(|path| std::fs::read(&path).map_err(|err| err.to_string()));

    report.heading("Install the helper");
    #[cfg(windows)]
    println!("\n   Windows will ask whether to let The Cabinet Helper make changes. Click Yes.");
    #[cfg(not(windows))]
    println!("\n   Your system will ask for your password to install The Cabinet's helper.");
    let installed = report
        .outcome(
            "the helper installs",
            helper::helper_install().map_err(|err| err.to_string()),
        )
        .is_some();
    let status = helper::helper_status();
    report.check(
        "the helper answers",
        status.ready,
        status.error.as_deref().unwrap_or(""),
    );
    if installed && status.ready {
        report.check(
            "the helper holds nothing yet",
            status.entries.is_empty(),
            "",
        );
        session(&mut report, &address);
        certificate(&mut report);
    }

    report.heading("Remove the helper");
    #[cfg(windows)]
    println!("\n   Windows will ask again. Click Yes.");
    #[cfg(not(windows))]
    println!("\n   Your system will ask for your password once more, to remove the helper.");
    report.outcome(
        "the helper uninstalls",
        helper::helper_uninstall().map_err(|err| err.to_string()),
    );
    report.check("the helper is gone", !helper::helper_status().installed, "");
    let left = leftovers();
    report.check(
        "nothing of the helper is left",
        left.is_empty(),
        &left.join(", "),
    );
    if let Ok(before) = hosts {
        let after =
            hosts_path().and_then(|path| std::fs::read(&path).map_err(|err| err.to_string()));
        report.check(
            "the hosts file is exactly as it was",
            after.as_ref() == Ok(&before),
            "",
        );
    }

    report.heading(&format!("{} problem(s)", report.failures));
    println!(
        "\nDone. Send {} back to the other person.",
        report.path.display()
    );
    pause("");
    if report.failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["probe", address] => probe(address),
        [] => guided(),
        _ => {
            println!("usage: friend-kit            (the guided check)\n       friend-kit probe <address>   (check another machine's port 987)");
            ExitCode::from(2)
        }
    }
}
