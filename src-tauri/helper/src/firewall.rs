use crate::protocol::{Port, Proto};
use std::path::{Component, Path, PathBuf};

pub trait AppFirewall: Send {
    fn enabled(&self) -> Result<bool, String>;
    fn listed(&self, program: &str) -> Result<bool, String>;
    fn allow(&self, program: &str) -> Result<(), String>;
    fn remove(&self, program: &str) -> Result<(), String>;
}

pub trait PortFirewall: Send {
    fn enabled(&self) -> Result<bool, String>;
    fn is_open(&self, port: &Port) -> Result<bool, String>;
    fn open(&self, port: &Port) -> Result<(), String>;
    fn close(&self, port: &Port) -> Result<(), String>;
}

pub trait RuleFirewall: Send {
    fn add(&self, name: &str, program: &str, ports: &[Port]) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
}

pub const TAILNET_INTERFACE: &str = "tailscale0";
pub const TAILNET_RANGE: &str = "100.64.0.0/10";

fn proto(proto: &Proto) -> &'static str {
    match proto {
        Proto::Tcp => "tcp",
        Proto::Udp => "udp",
    }
}

pub fn ufw_active(status: &str) -> bool {
    status
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "Status: active")
}

pub fn ufw_allows(status: &str, port: &Port) -> bool {
    let number = port.port.to_string();
    let exact = format!("{number}/{}", proto(&port.proto));
    status.lines().any(|line| {
        let line = line.split(" # ").next().unwrap_or("");
        let Some(at) = ["ALLOW", "DENY", "REJECT", "LIMIT"]
            .iter()
            .filter_map(|action| line.find(&format!(" {action}")))
            .min()
        else {
            return false;
        };
        let to = line[..at].trim();
        let mut words: Vec<&str> = line[at..].split_whitespace().collect();
        if words.first() != Some(&"ALLOW") {
            return false;
        }
        words.remove(0);
        match words.first() {
            Some(&"OUT") => return false,
            Some(&"IN") => {
                words.remove(0);
            }
            _ => {}
        }
        if words.join(" ") != "Anywhere" {
            return false;
        }
        let (target, interface) = match to.split_once(" on ") {
            Some((target, interface)) => (target.trim(), Some(interface.trim())),
            None => (to, None),
        };
        interface.is_none_or(|interface| interface == TAILNET_INTERFACE)
            && (target == exact || target == number || target == "Anywhere")
    })
}

pub const UFW_COMMENT: &str = "The Cabinet";

pub fn ufw_rule(verb: &[&str], port: &Port) -> Vec<String> {
    let mut args: Vec<String> = verb.iter().map(|word| word.to_string()).collect();
    args.extend(
        [
            "allow",
            "in",
            "on",
            TAILNET_INTERFACE,
            "to",
            "any",
            "port",
            &port.port.to_string(),
            "proto",
            proto(&port.proto),
            "comment",
            UFW_COMMENT,
        ]
        .iter()
        .map(|word| word.to_string()),
    );
    args
}

pub fn rule_name(id: &str) -> String {
    format!("{UFW_COMMENT} {id}")
}

pub fn netsh_rules(name: &str, program: &str, ports: &[Port]) -> Vec<Vec<String>> {
    let rule = |extra: Vec<String>| {
        let mut words: Vec<String> = [
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name=\"{name}\""),
            "dir=in",
            "action=allow",
            "enable=yes",
            "profile=any",
            &format!("program=\"{program}\""),
            &format!("remoteip={TAILNET_RANGE}"),
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        words.extend(extra);
        words
    };
    if ports.is_empty() {
        return vec![rule(Vec::new())];
    }
    [Proto::Tcp, Proto::Udp]
        .iter()
        .filter_map(|wanted| {
            let numbers: Vec<String> = ports
                .iter()
                .filter(|port| &port.proto == wanted)
                .map(|port| port.port.to_string())
                .collect();
            (!numbers.is_empty()).then(|| {
                rule(vec![
                    format!("protocol={}", proto(wanted).to_ascii_uppercase()),
                    format!("localport={}", numbers.join(",")),
                ])
            })
        })
        .collect()
}

pub struct Programs {
    pub firewall: Option<Box<dyn AppFirewall>>,
    pub port_firewall: Option<Box<dyn PortFirewall>>,
    pub rule_firewall: Option<Box<dyn RuleFirewall>>,
    pub low_ports: Option<Box<dyn crate::lowports::PortGrant>>,
    pub root: PathBuf,
    pub uid: Option<crate::server::User>,
}

pub fn path_text(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    #[cfg(windows)]
    let text = crate::win::plain(text);
    Some(text.to_string())
}

pub fn listed(output: &str) -> Vec<(String, bool)> {
    let mut apps = Vec::new();
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((number, path)) = line.split_once(" : ") else {
            continue;
        };
        if number.trim().parse::<u32>().is_err() {
            continue;
        }
        let allowed = lines
            .peek()
            .is_some_and(|next| next.trim().starts_with("(Allow"));
        apps.push((path.trim().to_string(), allowed));
    }
    apps
}

fn is_digest(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Programs {
    pub fn resolve(&self, program: &str) -> Result<String, String> {
        let refused = |why: &str| format!("\"{program}\" cannot be allowed: {why}");
        let root = self
            .root
            .canonicalize()
            .map_err(|_| refused("The Cabinet's data folder does not exist"))?;
        let path = Path::new(program)
            .canonicalize()
            .map_err(|_| refused("it does not exist"))?;
        let inside = path
            .strip_prefix(&root)
            .map_err(|_| refused("it is not a cartridge program in The Cabinet's store"))?;
        let names: Vec<&str> = inside
            .components()
            .map(|component| match component {
                Component::Normal(name) => name.to_str().unwrap_or(""),
                _ => "",
            })
            .collect();
        let in_store = names
            .windows(3)
            .any(|window| window[0] == "store" && is_digest(window[1]) && window[2] == "files");
        if !in_store || names.iter().any(|name| name.is_empty()) {
            return Err(refused(
                "it is not a cartridge program in The Cabinet's store",
            ));
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|_| refused("it does not exist"))?;
        if !metadata.is_file() {
            return Err(refused("it is not a regular file"));
        }
        #[cfg(unix)]
        if let Some(uid) = self.uid {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != uid {
                return Err(refused("it does not belong to The Cabinet's user"));
            }
        }
        #[cfg(windows)]
        if let Some(user) = &self.uid {
            let owner =
                crate::win::owner_of(&path).map_err(|_| refused("its owner cannot be read"))?;
            if &owner != user {
                return Err(refused("it does not belong to The Cabinet's user"));
            }
        }
        path_text(&path).ok_or_else(|| refused("its path is not text"))
    }
}

#[cfg(target_os = "macos")]
pub struct SocketFilter;

#[cfg(target_os = "macos")]
impl SocketFilter {
    const TOOL: &'static str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

    fn run(args: &[&str]) -> Result<String, String> {
        let output = std::process::Command::new(Self::TOOL)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run {}: {err}", Self::TOOL))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(text)
        } else {
            Err(format!(
                "{} {} failed: {} {}",
                Self::TOOL,
                args.join(" "),
                text.trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn state(program: &str) -> Result<Option<bool>, String> {
        Ok(listed(&Self::run(&["--listapps"])?)
            .into_iter()
            .find(|(path, _)| path == program)
            .map(|(_, allowed)| allowed))
    }
}

#[cfg(target_os = "macos")]
impl AppFirewall for SocketFilter {
    fn enabled(&self) -> Result<bool, String> {
        Ok(Self::run(&["--getglobalstate"])?.contains("enabled"))
    }

    fn listed(&self, program: &str) -> Result<bool, String> {
        Ok(Self::state(program)?.is_some())
    }

    fn allow(&self, program: &str) -> Result<(), String> {
        Self::run(&["--add", program])?;
        Self::run(&["--unblockapp", program])?;
        match Self::state(program)? {
            Some(true) => Ok(()),
            Some(false) => Err(format!("the firewall still blocks {program}")),
            None => Err(format!("the firewall did not take {program}")),
        }
    }

    fn remove(&self, program: &str) -> Result<(), String> {
        if Self::state(program)?.is_none() {
            return Ok(());
        }
        Self::run(&["--remove", program])?;
        match Self::state(program)? {
            None => Ok(()),
            Some(_) => Err(format!("the firewall kept {program}")),
        }
    }
}

#[cfg(target_os = "linux")]
pub struct Ufw;

#[cfg(target_os = "linux")]
impl Ufw {
    const TOOLS: &'static [&'static str] = &["/usr/sbin/ufw", "/usr/bin/ufw", "/sbin/ufw"];
    const FIREWALLD: &'static [&'static str] = &["/usr/bin/firewall-cmd", "/bin/firewall-cmd"];

    fn find(paths: &[&'static str]) -> Option<&'static str> {
        paths.iter().copied().find(|path| Path::new(path).exists())
    }

    fn run(tool: &str, args: &[String]) -> Result<String, String> {
        let output = std::process::Command::new(tool)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run {tool}: {err}"))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(text)
        } else {
            Err(format!(
                "{tool} {} failed: {} {}",
                args.join(" "),
                text.trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn status(tool: &str) -> Result<String, String> {
        Self::run(tool, &["status".to_string()])
    }

    fn tool() -> Result<&'static str, String> {
        Self::find(Self::TOOLS).ok_or_else(|| "ufw is not installed".to_string())
    }

    fn firewalld_running() -> bool {
        Self::find(Self::FIREWALLD).is_some_and(|tool| {
            Self::run(tool, &["--state".to_string()]).is_ok_and(|state| state.trim() == "running")
        })
    }
}

#[cfg(target_os = "linux")]
impl PortFirewall for Ufw {
    fn enabled(&self) -> Result<bool, String> {
        if let Some(tool) = Self::find(Self::TOOLS) {
            if ufw_active(&Self::status(tool)?) {
                return Ok(true);
            }
        }
        if Self::firewalld_running() {
            return Err(
                "firewalld is running, and The Cabinet cannot open ports in it yet".to_string(),
            );
        }
        Ok(false)
    }

    fn is_open(&self, port: &Port) -> Result<bool, String> {
        Ok(ufw_allows(&Self::status(Self::tool()?)?, port))
    }

    fn open(&self, port: &Port) -> Result<(), String> {
        let tool = Self::tool()?;
        Self::run(tool, &ufw_rule(&[], port))?;
        if self.is_open(port)? {
            Ok(())
        } else {
            Err(format!("ufw did not open port {}", port.port))
        }
    }

    fn close(&self, port: &Port) -> Result<(), String> {
        Self::run(Self::tool()?, &ufw_rule(&["delete"], port)).map(|_| ())
    }
}

#[cfg(windows)]
pub struct Netsh;

#[cfg(windows)]
impl Netsh {
    fn run(words: &[String]) -> Result<(bool, String), String> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let tool = crate::win::windows_dir()?.join(r"System32\netsh.exe");
        let output = std::process::Command::new(&tool)
            .raw_arg(words.join(" "))
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|err| format!("cannot run {}: {err}", tool.display()))?;
        let text = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok((output.status.success(), text.trim().to_string()))
    }

    fn named(verb: &str, name: &str) -> Vec<String> {
        [
            "advfirewall",
            "firewall",
            verb,
            "rule",
            &format!("name=\"{name}\""),
        ]
        .iter()
        .map(|word| word.to_string())
        .collect()
    }

    pub fn exists(name: &str) -> Result<bool, String> {
        Ok(Self::run(&Self::named("show", name))?.0)
    }
}

#[cfg(windows)]
impl RuleFirewall for Netsh {
    fn add(&self, name: &str, program: &str, ports: &[Port]) -> Result<(), String> {
        for rule in netsh_rules(name, program, ports) {
            let (added, text) = Self::run(&rule)?;
            if !added {
                if let Err(err) = self.remove(name) {
                    log::error!("cannot remove the rule {name} again: {err}");
                }
                return Err(format!("the firewall did not take the rule {name}: {text}"));
            }
        }
        if Self::exists(name)? {
            Ok(())
        } else {
            Err(format!("the firewall does not show the rule {name}"))
        }
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        if !Self::exists(name)? {
            return Ok(());
        }
        let (_, text) = Self::run(&Self::named("delete", name))?;
        if Self::exists(name)? {
            Err(format!("the firewall kept the rule {name}: {text}"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code, unused_imports))]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub(crate) type Rule = (String, String, Vec<Port>);

    #[derive(Clone, Default)]
    pub(crate) struct FakeRules {
        pub rules: Arc<Mutex<Vec<Rule>>>,
        pub refuse: Arc<Mutex<bool>>,
    }

    impl RuleFirewall for FakeRules {
        fn add(&self, name: &str, program: &str, ports: &[Port]) -> Result<(), String> {
            if *self.refuse.lock().unwrap() {
                return Err("the firewall refused".to_string());
            }
            self.rules.lock().unwrap().push((
                name.to_string(),
                program.to_string(),
                ports.to_vec(),
            ));
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), String> {
            self.rules.lock().unwrap().retain(|(held, ..)| held != name);
            Ok(())
        }
    }

    #[test]
    fn a_windows_rule_admits_the_program_from_the_tailnet_on_its_ports() {
        let program = r"C:\Users\me\AppData\Roaming\com.the-cabinet.app\x\echo-cart.exe";
        let common = format!(
            "advfirewall firewall add rule name=\"The Cabinet 7\" dir=in action=allow enable=yes profile=any program=\"{program}\" remoteip=100.64.0.0/10"
        );
        let ports = [
            Port {
                proto: Proto::Udp,
                port: 3074,
            },
            Port {
                proto: Proto::Tcp,
                port: 987,
            },
            Port {
                proto: Proto::Tcp,
                port: 443,
            },
        ];
        let rules: Vec<String> = netsh_rules(&rule_name("7"), program, &ports)
            .iter()
            .map(|words| words.join(" "))
            .collect();
        assert_eq!(
            rules,
            vec![
                format!("{common} protocol=TCP localport=987,443"),
                format!("{common} protocol=UDP localport=3074"),
            ]
        );
        let rules: Vec<String> = netsh_rules(&rule_name("7"), program, &[])
            .iter()
            .map(|words| words.join(" "))
            .collect();
        assert_eq!(rules, vec![common]);
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakeFirewall {
        pub apps: Arc<Mutex<Vec<String>>>,
        pub off: Arc<Mutex<bool>>,
    }

    impl AppFirewall for FakeFirewall {
        fn enabled(&self) -> Result<bool, String> {
            Ok(!*self.off.lock().unwrap())
        }

        fn listed(&self, program: &str) -> Result<bool, String> {
            Ok(self.apps.lock().unwrap().iter().any(|app| app == program))
        }

        fn allow(&self, program: &str) -> Result<(), String> {
            self.apps.lock().unwrap().push(program.to_string());
            Ok(())
        }

        fn remove(&self, program: &str) -> Result<(), String> {
            self.apps.lock().unwrap().retain(|app| app != program);
            Ok(())
        }
    }

    fn tcp(port: u16) -> Port {
        Port {
            proto: Proto::Tcp,
            port,
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakePorts {
        pub open: Arc<Mutex<Vec<Port>>>,
        pub off: Arc<Mutex<bool>>,
    }

    impl PortFirewall for FakePorts {
        fn enabled(&self) -> Result<bool, String> {
            Ok(!*self.off.lock().unwrap())
        }

        fn is_open(&self, port: &Port) -> Result<bool, String> {
            Ok(self.open.lock().unwrap().contains(port))
        }

        fn open(&self, port: &Port) -> Result<(), String> {
            self.open.lock().unwrap().push(port.clone());
            Ok(())
        }

        fn close(&self, port: &Port) -> Result<(), String> {
            self.open.lock().unwrap().retain(|open| open != port);
            Ok(())
        }
    }

    #[test]
    fn reads_which_ports_ufw_allows_on_the_tailnet() {
        let status = "Status: active

To                         Action      From
--                         ------      ----
22/tcp                     ALLOW       Anywhere
987/tcp on tailscale0      ALLOW       Anywhere                   # The Cabinet
5000                       ALLOW       192.0.2.100
6000/udp on eth0           ALLOW       Anywhere
7000/tcp                   DENY        Anywhere
8000/tcp                   ALLOW OUT   Anywhere
987/tcp (v6) on tailscale0 ALLOW       Anywhere (v6)              # The Cabinet
";
        assert!(ufw_active(status));
        assert!(!ufw_active("Status: inactive\n"));
        assert!(ufw_allows(status, &tcp(22)));
        assert!(ufw_allows(status, &tcp(987)));
        assert!(!ufw_allows(
            status,
            &Port {
                proto: Proto::Udp,
                port: 987
            }
        ));
        for closed in [5000, 6000, 7000, 8000, 55435] {
            assert!(!ufw_allows(status, &tcp(closed)), "{closed}");
        }
        assert!(ufw_allows(
            "Anywhere on tailscale0     ALLOW IN    Anywhere\n",
            &tcp(55435)
        ));
        assert_eq!(
            ufw_rule(&["delete"], &tcp(987)).join(" "),
            "delete allow in on tailscale0 to any port 987 proto tcp comment The Cabinet"
        );
    }

    #[test]
    fn reads_the_firewall_app_list() {
        let output = "Total number of apps = 3 \n1 : /Applications/Some App.app \n             (Allow incoming connections)\n2 : /usr/bin/ruby \n             (Block incoming connections)\n3 : /usr/sbin/cupsd \n";
        assert_eq!(
            listed(output),
            vec![
                ("/Applications/Some App.app".to_string(), true),
                ("/usr/bin/ruby".to_string(), false),
                ("/usr/sbin/cupsd".to_string(), false),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_cartridge_programs_in_the_store_resolve() {
        let scratch = crate::engine::tests::Scratch::new("programs");
        let digest = "ab".repeat(32);
        let files = scratch.0.join("store").join(&digest).join("files");
        std::fs::create_dir_all(files.join("macos-aarch64")).unwrap();
        let program = files.join("macos-aarch64/echo-cart");
        std::fs::write(&program, "x").unwrap();
        std::fs::write(scratch.0.join("loose"), "x").unwrap();
        std::fs::create_dir_all(scratch.0.join("store/short/files")).unwrap();
        std::fs::write(scratch.0.join("store/short/files/x"), "x").unwrap();
        let programs = Programs {
            firewall: Some(Box::new(FakeFirewall::default())),
            port_firewall: None,
            rule_firewall: None,
            low_ports: None,
            root: scratch.0.clone(),
            uid: None,
        };
        let resolved = programs.resolve(program.to_str().unwrap()).unwrap();
        assert_eq!(Path::new(&resolved), program.canonicalize().unwrap());
        for bad in [
            scratch.0.join("loose"),
            scratch.0.join("store/short/files/x"),
            files.join("macos-aarch64"),
            files.join("missing"),
            PathBuf::from("/bin/sh"),
            files.join("macos-aarch64/../../../../loose"),
        ] {
            assert!(programs.resolve(bad.to_str().unwrap()).is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/bin/sh", files.join("link")).unwrap();
            assert!(programs
                .resolve(files.join("link").to_str().unwrap())
                .is_err());
            let other = Programs {
                firewall: Some(Box::new(FakeFirewall::default())),
                port_firewall: None,
                rule_firewall: None,
                low_ports: None,
                root: scratch.0.clone(),
                uid: Some(u32::MAX - 7),
            };
            assert!(other.resolve(program.to_str().unwrap()).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_program_resolves_to_a_plain_path_only_for_its_owner() {
        let scratch = crate::engine::tests::Scratch::new("programs-win");
        let program = crate::engine::tests::program(&scratch);
        let me = crate::win::current_user().unwrap();
        crate::win::set_owner(Path::new(&program), &me).unwrap();
        let programs = |uid: &str| Programs {
            firewall: None,
            port_firewall: None,
            rule_firewall: Some(Box::new(FakeRules::default())),
            low_ports: None,
            root: scratch.0.clone(),
            uid: Some(uid.to_string()),
        };
        let resolved = programs(&me).resolve(&program).unwrap();
        assert_eq!(resolved, program);
        assert!(!resolved.starts_with(r"\\?\"));
        assert!(programs(crate::win::SYSTEM).resolve(&program).is_err());
        assert!(programs(&me)
            .resolve(r"C:\Windows\System32\cmd.exe")
            .is_err());
    }
}
