use crate::contracts::LaunchSpec;
use cabinet_cartridge::manifest::{Manifest, StepKind};
use cabinet_cartridge::vars::{self, Var};
use cabinet_cartridge::Platform;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const ASSETS_DIR: &str = "assets";

pub fn bundled(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

pub struct Context<'a> {
    pub manifest: &'a Manifest,
    pub cartridge_dir: PathBuf,
    pub data_dir: PathBuf,
    pub platform: Platform,
    pub settings: &'a Map<String, Value>,
}

impl Context<'_> {
    pub fn asset_path(&self, id: &str) -> Result<PathBuf, String> {
        let asset = self
            .manifest
            .asset(id)
            .ok_or_else(|| format!("asset \"{id}\" is not declared"))?;
        match &asset.file {
            Some(file) => Ok(bundled(&self.cartridge_dir, file)),
            None => Ok(self.data_dir.join(ASSETS_DIR).join(id)),
        }
    }

    fn resolve(&self, var: Var<'_>) -> Result<String, String> {
        let path = |path: &Path| path.to_string_lossy().into_owned();
        match var {
            Var::Cartridge => Ok(path(&self.cartridge_dir)),
            Var::Data => Ok(path(&self.data_dir)),
            Var::Platform => Ok(self.platform.to_string()),
            Var::Asset(id) => self.asset_path(id).map(|asset| path(&asset)),
            Var::Setting(key) => {
                let label = self
                    .manifest
                    .setting(key)
                    .map_or(key, |setting| setting.label.as_str());
                match self.settings.get(key) {
                    Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
                    Some(Value::Bool(flag)) => Ok(flag.to_string()),
                    _ => Err(format!("the setting \"{label}\" is not set")),
                }
            }
        }
    }

    pub fn expand(&self, template: &str) -> Result<String, String> {
        vars::expand(template, |var| self.resolve(var))
    }

    pub fn expand_path(&self, template: &str) -> Result<PathBuf, String> {
        let expanded = PathBuf::from(self.expand(template)?);
        if expanded.is_absolute() {
            Ok(expanded)
        } else {
            Err(format!(
                "\"{template}\" does not expand to an absolute path ({})",
                expanded.display()
            ))
        }
    }

    pub fn program(&self, kind: StepKind) -> Result<Option<LaunchSpec>, String> {
        let Some(step) = self.manifest.lifecycle.step(kind) else {
            return Ok(None);
        };
        let run = self
            .manifest
            .program_path(&step.program, self.platform)
            .ok_or_else(|| {
                format!(
                    "program \"{}\" has no build for {}",
                    step.program, self.platform
                )
            })?;
        let program = &self.manifest.programs[&step.program];
        let args = program
            .args
            .iter()
            .chain(&step.args)
            .map(|arg| self.expand(arg))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(LaunchSpec {
            program: bundled(&self.cartridge_dir, run),
            args,
            cwd: self.data_dir.clone(),
            envs: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> Manifest {
        let extra = r#"
[lifecycle.setup]
program = "main"
args = ["setup", "${asset:bundled}", "${setting:dir}", "${platform}", "${data}"]

[[assets]]
id = "bundled"
file = "files/b.bin"

[[assets]]
id = "fetched"
[assets.download.linux-x86_64]
url = "https://example.com/f"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[settings]]
key = "dir"
label = "Game folder"
type = "dir"

[[settings]]
key = "flag"
label = "Flag"
type = "bool"
"#;
        let text = crate::cartridge::testing::manifest("ctx", "1.0.0", &["linux-x86_64"], extra);
        Manifest::parse(&text, None).unwrap()
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join("cabinet-ctx")
    }

    #[test]
    fn expands_variables_into_paths_and_args() {
        let manifest = manifest();
        let settings = Map::from_iter([
            (
                "dir".to_string(),
                json!(root().join("games").to_string_lossy()),
            ),
            ("flag".to_string(), json!(true)),
        ]);
        let context = Context {
            manifest: &manifest,
            cartridge_dir: root().join("store").join("abc"),
            data_dir: root().join("data"),
            platform: Platform::LinuxX86_64,
            settings: &settings,
        };
        assert_eq!(
            context.asset_path("fetched").unwrap(),
            root().join("data").join("assets").join("fetched")
        );
        assert_eq!(context.expand("${setting:flag}").unwrap(), "true");
        let setup = context.program(StepKind::Setup).unwrap().unwrap();
        assert_eq!(
            setup.program,
            root().join("store").join("abc").join("files").join("main")
        );
        assert_eq!(setup.cwd, root().join("data"));
        assert_eq!(setup.args[0], "setup");
        assert_eq!(
            PathBuf::from(&setup.args[1]),
            root().join("store").join("abc").join("files").join("b.bin")
        );
        assert_eq!(setup.args[3], "linux-x86_64");
        assert!(context.program(StepKind::Service).unwrap().is_none());
        assert!(context.expand_path("relative/${platform}").is_err());
    }

    #[test]
    fn names_the_missing_setting() {
        let manifest = manifest();
        let settings = Map::new();
        let context = Context {
            manifest: &manifest,
            cartridge_dir: root(),
            data_dir: root(),
            platform: Platform::LinuxX86_64,
            settings: &settings,
        };
        let err = context.program(StepKind::Setup).unwrap_err();
        assert_eq!(err, "the setting \"Game folder\" is not set");
    }
}
