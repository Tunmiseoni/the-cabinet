use crate::error::{Error, Result};
use crate::platform::Platform;
use crate::version::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FORMAT: u32 = 1;
pub const MANIFEST_PATH: &str = "cartridge.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub id: String,
    pub version: Version,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub min_cabinet: Version,
    pub platforms: Vec<Platform>,
    #[serde(default)]
    pub room: RoomSpec,
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub programs: BTreeMap<String, Program>,
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub touches: Vec<Touch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LateJoin {
    Never,
    #[default]
    Always,
    Ask,
}

fn default_max_members() -> u32 {
    8
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSpec {
    #[serde(default = "default_max_members")]
    pub max_members: u32,
    #[serde(default)]
    pub late_join: LateJoin,
}

impl Default for RoomSpec {
    fn default() -> Self {
        Self {
            max_members: default_max_members(),
            late_join: LateJoin::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    Dir,
    File,
    String,
    Bool,
    Choice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: SettingType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub detect: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProgramKind {
    #[default]
    Native,
    Wasm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    #[serde(default)]
    pub kind: ProgramKind,
    #[serde(default)]
    pub args: Vec<String>,
    pub run: BTreeMap<Platform, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Setup,
    Session,
    Service,
    Uninstall,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Setup => "setup",
            StepKind::Session => "session",
            StepKind::Service => "service",
            StepKind::Uninstall => "uninstall",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    #[serde(default)]
    pub setup: Option<Step>,
    pub session: Step,
    #[serde(default)]
    pub service: Option<Step>,
    #[serde(default)]
    pub uninstall: Option<Step>,
}

impl Lifecycle {
    pub fn step(&self, kind: StepKind) -> Option<&Step> {
        match kind {
            StepKind::Setup => self.setup.as_ref(),
            StepKind::Session => Some(&self.session),
            StepKind::Service => self.service.as_ref(),
            StepKind::Uninstall => self.uninstall.as_ref(),
        }
    }

    pub fn steps(&self) -> impl Iterator<Item = (StepKind, &Step)> {
        [
            StepKind::Setup,
            StepKind::Session,
            StepKind::Service,
            StepKind::Uninstall,
        ]
        .into_iter()
        .filter_map(|kind| self.step(kind).map(|step| (kind, step)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Download {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub id: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub download: Option<BTreeMap<Platform, Download>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub label: String,
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostsTarget {
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosts {
    pub names: Vec<String>,
    pub target: HostsTarget,
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    System,
    Bottle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustCa {
    pub asset: String,
    pub permitted_names: Vec<String>,
    pub scope: Scope,
    pub stores: Vec<Store>,
    #[serde(default)]
    pub bottle_setting: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub proto: Proto,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firewall {
    pub scope: Scope,
    pub inbound: Vec<Port>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub low_ports: Vec<u16>,
    #[serde(default)]
    pub hosts: Option<Hosts>,
    #[serde(default)]
    pub trust_ca: Option<TrustCa>,
    #[serde(default)]
    pub firewall: Option<Firewall>,
}

impl Capabilities {
    pub fn is_empty(&self) -> bool {
        self == &Capabilities::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Touch {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

impl Touch {
    pub fn applies_to(&self, platform: Platform) -> bool {
        self.platforms
            .as_ref()
            .is_none_or(|platforms| platforms.contains(&platform))
    }
}

#[derive(Deserialize)]
struct Header {
    format: Option<i64>,
    min_cabinet: Option<String>,
}

impl Manifest {
    pub fn parse(text: &str, cabinet: Option<&Version>) -> Result<Manifest> {
        let header: Header = toml::from_str(text)?;
        match header.format {
            Some(format) if format == i64::from(FORMAT) => {}
            Some(format) => return Err(Error::UnsupportedFormat(format)),
            None => return Err(Error::Invalid(vec!["`format` is missing".to_string()])),
        }
        if let (Some(cabinet), Some(Ok(needed))) = (
            cabinet,
            header.min_cabinet.as_deref().map(str::parse::<Version>),
        ) {
            if &needed > cabinet {
                return Err(Error::NeedsNewerCabinet {
                    needed,
                    current: cabinet.clone(),
                });
            }
        }
        let manifest: Manifest = toml::from_str(text)?;
        let problems = crate::validate::problems(&manifest);
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(Error::Invalid(problems))
        }
    }

    pub fn supports(&self, platform: Platform) -> bool {
        self.platforms.contains(&platform)
    }

    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|setting| setting.key == key)
    }

    pub fn asset(&self, id: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.id == id)
    }

    pub fn program_path(&self, program: &str, platform: Platform) -> Option<&str> {
        self.programs
            .get(program)
            .and_then(|program| program.run.get(&platform))
            .map(String::as_str)
    }

    pub fn program_paths(&self) -> std::collections::BTreeSet<&str> {
        self.programs
            .values()
            .flat_map(|program| program.run.values().map(String::as_str))
            .collect()
    }

    pub fn bundled_paths(&self) -> Vec<&str> {
        let assets = self.assets.iter().filter_map(|asset| asset.file.as_deref());
        self.program_paths().into_iter().chain(assets).collect()
    }
}
