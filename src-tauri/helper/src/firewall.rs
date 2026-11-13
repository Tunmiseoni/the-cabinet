use std::path::{Component, Path, PathBuf};

pub trait AppFirewall: Send {
    fn enabled(&self) -> Result<bool, String>;
    fn listed(&self, program: &str) -> Result<bool, String>;
    fn allow(&self, program: &str) -> Result<(), String>;
    fn remove(&self, program: &str) -> Result<(), String>;
}

pub struct Programs {
    pub firewall: Box<dyn AppFirewall>,
    pub root: PathBuf,
    pub uid: Option<u32>,
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
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| refused("its path is not text"))
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

#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code, unused_imports))]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
            firewall: Box::new(FakeFirewall::default()),
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
                firewall: Box::new(FakeFirewall::default()),
                root: scratch.0.clone(),
                uid: Some(u32::MAX - 7),
            };
            assert!(other.resolve(program.to_str().unwrap()).is_err());
        }
    }
}
