use crate::manifest::{Asset, Capabilities, Manifest, ProgramKind, SettingType, Store, Touch};
use crate::platform::Platform;
use crate::vars::{self, Var};
use std::collections::BTreeSet;

pub const DETECTORS: [&str; 1] = ["retroarch"];
pub const MAX_ID_LEN: usize = 64;

pub fn is_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ID_LEN
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn is_bundled_path(value: &str) -> bool {
    value.starts_with("files/")
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn is_dns_name(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').count() >= 2
        && value.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
                && bytes[0] != b'-'
                && bytes[bytes.len() - 1] != b'-'
        })
        && !value
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

struct Check<'a> {
    manifest: &'a Manifest,
    problems: Vec<String>,
}

impl Check<'_> {
    fn fail(&mut self, problem: impl Into<String>) {
        self.problems.push(problem.into());
    }

    fn platforms(&mut self, what: &str, platforms: impl IntoIterator<Item = Platform>) {
        for platform in platforms {
            if !self.manifest.supports(platform) {
                self.fail(format!(
                    "{what} names {platform}, which is not in `platforms`"
                ));
            }
        }
    }

    fn template(&mut self, what: &str, template: &str, allowed: &[&str]) {
        let found = match vars::vars(template) {
            Ok(found) => found,
            Err(err) => return self.fail(format!("{what}: {err}")),
        };
        for var in found {
            let name = match var {
                Var::Cartridge => "cartridge",
                Var::Data => "data",
                Var::Platform => "platform",
                Var::Setting(key) => {
                    if self.manifest.setting(key).is_none() {
                        self.fail(format!("{what}: setting \"{key}\" is not declared"));
                    }
                    "setting"
                }
                Var::Asset(id) => {
                    if self.manifest.asset(id).is_none() {
                        self.fail(format!("{what}: asset \"{id}\" is not declared"));
                    }
                    "asset"
                }
            };
            if !allowed.contains(&name) {
                self.fail(format!(
                    "{what}: ${{{name}}} variables are not allowed here"
                ));
            }
        }
    }

    fn top_level(&mut self) {
        let manifest = self.manifest;
        if !is_name(&manifest.id) {
            self.fail(format!(
                "id \"{}\" must be 1-{MAX_ID_LEN} characters of a-z, 0-9, '-' or '_', starting with a letter or digit",
                manifest.id
            ));
        }
        if manifest.title.trim().is_empty() {
            self.fail("title is empty");
        }
        if manifest.platforms.is_empty() {
            self.fail("`platforms` is empty");
        }
        let unique: BTreeSet<_> = manifest.platforms.iter().collect();
        if unique.len() != manifest.platforms.len() {
            self.fail("`platforms` lists a platform twice");
        }
        if manifest.room.max_members == 0 {
            self.fail("room.max_members must be at least 1");
        }
    }

    fn settings(&mut self) {
        let mut seen = BTreeSet::new();
        for setting in &self.manifest.settings {
            let what = format!("setting \"{}\"", setting.key);
            if !is_name(&setting.key) {
                self.fail(format!("{what}: the key is not a valid name"));
            }
            if !seen.insert(setting.key.as_str()) {
                self.fail(format!("{what} is declared twice"));
            }
            if setting.label.trim().is_empty() {
                self.fail(format!("{what}: label is empty"));
            }
            match (setting.kind, setting.options.is_empty()) {
                (SettingType::Choice, true) => {
                    self.fail(format!("{what}: a choice needs `options`"));
                }
                (SettingType::Choice, false) => {}
                (_, false) => self.fail(format!("{what}: only a choice takes `options`")),
                (_, true) => {}
            }
            if let Some(detect) = &setting.detect {
                if !DETECTORS.contains(&detect.as_str()) {
                    self.fail(format!(
                        "{what}: unknown detector \"{detect}\" (known: {})",
                        DETECTORS.join(", ")
                    ));
                }
                if !matches!(setting.kind, SettingType::File | SettingType::Dir) {
                    self.fail(format!("{what}: a detector needs a file or dir setting"));
                }
            }
        }
    }

    fn programs(&mut self) {
        let manifest = self.manifest;
        for (name, program) in &manifest.programs {
            let what = format!("program \"{name}\"");
            if !is_name(name) {
                self.fail(format!("{what}: the name is not a valid name"));
            }
            if program.kind == ProgramKind::Wasm {
                self.fail(format!(
                    "{what}: the wasm kind is not supported by this Cabinet"
                ));
            }
            self.platforms(&what, program.run.keys().copied());
            for path in program.run.values() {
                if !is_bundled_path(path) {
                    self.fail(format!(
                        "{what}: \"{path}\" must be a relative path under files/"
                    ));
                }
            }
            for arg in &program.args {
                self.template(
                    &what,
                    arg,
                    &["cartridge", "data", "platform", "setting", "asset"],
                );
            }
        }
        let mut used = BTreeSet::new();
        for (kind, step) in manifest.lifecycle.steps() {
            let what = format!("lifecycle.{}", kind.as_str());
            match manifest.programs.get(&step.program) {
                None => self.fail(format!(
                    "{what}: program \"{}\" is not declared",
                    step.program
                )),
                Some(program) => {
                    if used.insert(step.program.as_str()) {
                        for platform in &manifest.platforms {
                            if !program.run.contains_key(platform) {
                                self.fail(format!(
                                    "program \"{}\" has no `run` entry for {platform}",
                                    step.program
                                ));
                            }
                        }
                    }
                }
            }
            for arg in &step.args {
                self.template(
                    &what,
                    arg,
                    &["cartridge", "data", "platform", "setting", "asset"],
                );
            }
        }
    }

    fn asset(&mut self, asset: &Asset) {
        let what = format!("asset \"{}\"", asset.id);
        match (&asset.file, &asset.download) {
            (Some(file), None) => {
                if !is_bundled_path(file) {
                    self.fail(format!(
                        "{what}: \"{file}\" must be a relative path under files/"
                    ));
                }
            }
            (None, Some(downloads)) => {
                self.platforms(&what, downloads.keys().copied());
                for platform in &self.manifest.platforms {
                    if !downloads.contains_key(platform) {
                        self.fail(format!("{what} has no download for {platform}"));
                    }
                }
                for (platform, download) in downloads {
                    if !download.url.starts_with("https://") {
                        self.fail(format!("{what}: the {platform} url must be https"));
                    }
                    if !is_sha256(&download.sha256) {
                        self.fail(format!(
                            "{what}: the {platform} sha256 must be 64 lowercase hex digits"
                        ));
                    }
                }
            }
            _ => self.fail(format!("{what} needs exactly one of `file` or `download`")),
        }
    }

    fn assets_and_checks(&mut self) {
        let mut seen = BTreeSet::new();
        for asset in &self.manifest.assets {
            if !is_name(&asset.id) {
                self.fail(format!(
                    "asset \"{}\": the id is not a valid name",
                    asset.id
                ));
            }
            if !seen.insert(asset.id.as_str()) {
                self.fail(format!("asset \"{}\" is declared twice", asset.id));
            }
            self.asset(asset);
        }
        for check in &self.manifest.checks {
            let what = format!("check \"{}\"", check.label);
            if check.label.trim().is_empty() {
                self.fail("a check has an empty label");
            }
            self.template(
                &what,
                &check.path,
                &["cartridge", "data", "setting", "asset"],
            );
            if !is_sha256(&check.sha256) {
                self.fail(format!("{what}: sha256 must be 64 lowercase hex digits"));
            }
        }
    }

    fn capabilities(&mut self, capabilities: &Capabilities) {
        let mut ports = BTreeSet::new();
        for port in &capabilities.low_ports {
            if *port == 0 || *port >= 1024 {
                self.fail(format!(
                    "capabilities.low_ports: {port} is not a privileged port (1-1023)"
                ));
            }
            if !ports.insert(port) {
                self.fail(format!("capabilities.low_ports lists {port} twice"));
            }
        }
        if !capabilities.low_ports.is_empty() && self.manifest.lifecycle.service.is_none() {
            self.fail("capabilities.low_ports needs a lifecycle.service");
        }
        if let Some(hosts) = &capabilities.hosts {
            if hosts.names.is_empty() {
                self.fail("capabilities.hosts.names is empty");
            }
            for name in &hosts.names {
                if !is_dns_name(name) {
                    self.fail(format!(
                        "capabilities.hosts: \"{name}\" is not a lowercase DNS name"
                    ));
                }
            }
            if hosts.scope != crate::manifest::Scope::Session {
                self.fail("capabilities.hosts.scope must be \"session\"");
            }
            if self.manifest.lifecycle.service.is_none() {
                self.fail(
                    "capabilities.hosts targets the service, but there is no lifecycle.service",
                );
            }
        }
        if let Some(ca) = &capabilities.trust_ca {
            match self.manifest.asset(&ca.asset) {
                None => self.fail(format!(
                    "capabilities.trust_ca: asset \"{}\" is not declared",
                    ca.asset
                )),
                Some(asset) if asset.file.is_none() => self.fail(format!(
                    "capabilities.trust_ca: asset \"{}\" must be bundled (`file`)",
                    ca.asset
                )),
                Some(_) => {}
            }
            if ca.permitted_names.is_empty() {
                self.fail("capabilities.trust_ca.permitted_names is empty");
            }
            for name in &ca.permitted_names {
                if !is_dns_name(name) {
                    self.fail(format!(
                        "capabilities.trust_ca: \"{name}\" is not a registrable lowercase DNS name"
                    ));
                }
            }
            if ca.stores.is_empty() {
                self.fail("capabilities.trust_ca.stores is empty");
            }
            let bottle = ca.stores.contains(&Store::Bottle);
            match (&ca.bottle_setting, bottle) {
                (None, true) => {
                    self.fail("capabilities.trust_ca: the bottle store needs `bottle_setting`")
                }
                (Some(_), false) => self.fail(
                    "capabilities.trust_ca: `bottle_setting` is only used with the bottle store",
                ),
                (Some(key), true) => match self.manifest.setting(key) {
                    Some(setting) if setting.kind == SettingType::Dir => {}
                    Some(_) => self.fail(format!(
                        "capabilities.trust_ca: setting \"{key}\" must be a dir"
                    )),
                    None => self.fail(format!(
                        "capabilities.trust_ca: setting \"{key}\" is not declared"
                    )),
                },
                (None, false) => {}
            }
        }
        if let Some(firewall) = &capabilities.firewall {
            if firewall.inbound.is_empty() {
                self.fail("capabilities.firewall.inbound is empty");
            }
            for port in &firewall.inbound {
                if port.port == 0 {
                    self.fail("capabilities.firewall: port 0 is not a port");
                }
            }
        }
    }

    fn touch(&mut self, touch: &Touch) {
        if let Some(platforms) = &touch.platforms {
            self.platforms("a touch", platforms.iter().copied());
            if platforms.is_empty() {
                self.fail("a touch has an empty `platforms`");
            }
        }
        match (&touch.path, &touch.registry) {
            (Some(path), None) => {
                let what = format!("touch \"{path}\"");
                self.template(&what, path, &["setting"]);
            }
            (None, Some(key)) => {
                let what = format!("touch \"{key}\"");
                let windows_only = touch
                    .platforms
                    .as_ref()
                    .is_some_and(|platforms| platforms.iter().all(|p| p.is_windows()));
                if !windows_only {
                    self.fail(format!(
                        "{what}: a registry touch needs `platforms` limited to Windows"
                    ));
                }
                let upper = key.to_ascii_uppercase();
                if !(upper.starts_with("HKCU\\") || upper.starts_with("HKEY_CURRENT_USER\\")) {
                    self.fail(format!("{what}: only HKCU keys can be declared"));
                }
            }
            _ => self.fail("a touch needs exactly one of `path` or `registry`"),
        }
    }
}

pub fn problems(manifest: &Manifest) -> Vec<String> {
    let mut check = Check {
        manifest,
        problems: Vec::new(),
    };
    check.top_level();
    check.settings();
    check.programs();
    check.assets_and_checks();
    check.capabilities(&manifest.capabilities);
    for touch in &manifest.touches {
        check.touch(touch);
    }
    check.problems
}
