use super::assets::{self, Fetch};
use super::context::Context;
use super::footprint;
use super::registry::{self, CartridgeRef};
use super::store::{Installed, Staged, Store};
use crate::supervisor::{self, Capture, Stdin};
use cabinet_cartridge::manifest::{Manifest, StepKind};
use cabinet_cartridge::{Platform, Version};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Installer<'a> {
    pub store: &'a mut Store,
    pub cabinet: Version,
    pub platform: Platform,
    pub fetch: &'a dyn Fetch,
    pub log_dir: PathBuf,
    pub now: u64,
    pub busy: bool,
}

pub fn run_step(
    context: &Context<'_>,
    kind: StepKind,
    log_dir: &Path,
    timeout: Duration,
) -> crate::error::Result<()> {
    let Some(spec) = context.program(kind)? else {
        return Ok(());
    };
    let label = format!("{}[{}]", context.manifest.id, kind.as_str());
    let log = log_dir.join(format!(
        "cartridge-{}-{}.log",
        context.manifest.id,
        kind.as_str()
    ));
    let capture = Capture {
        label: label.clone(),
        sink: crate::logging::open_log(&log)?,
        observer: None,
        stdout: None,
    };
    log::info!("running {label}: {}", spec.program.display());
    let child = supervisor::spawn_with(&spec, Some(capture), Stdin::Piped)?;
    child.close_stdin();
    match child.wait_blocking(timeout) {
        Some(exit) if exit.code == Some(0) => Ok(()),
        Some(exit) => Err(format!(
            "{label} failed (exit {}); see {}",
            exit.code
                .map_or_else(|| "by signal".to_string(), |code| code.to_string()),
            log.display()
        )
        .into()),
        None => {
            child.stop_blocking(Duration::ZERO);
            Err(format!("{label} did not finish within {}s", timeout.as_secs()).into())
        }
    }
}

fn effective_settings(
    store: &Store,
    staged: &Staged,
    given: Option<Map<String, Value>>,
) -> crate::error::Result<Map<String, Value>> {
    let manifest = &staged.inspected.manifest;
    let settings = given.unwrap_or_else(|| {
        store
            .registry()
            .entry(&staged.cartridge)
            .map(|entry| entry.settings.clone())
            .unwrap_or_default()
    });
    registry::check_settings(manifest, &settings)?;
    let missing = registry::missing_required(manifest, &settings);
    if !missing.is_empty() {
        return Err(format!(
            "{} needs these settings first: {}",
            manifest.title,
            missing.join(", ")
        )
        .into());
    }
    Ok(settings)
}

impl Installer<'_> {
    fn prepare(&self, staged: &Staged, settings: &Map<String, Value>) -> crate::error::Result<()> {
        let manifest = &staged.inspected.manifest;
        let data_dir = self.store.data_dir(&staged.cartridge);
        std::fs::create_dir_all(&data_dir)?;
        let context = Context {
            manifest,
            cartridge_dir: self.store.unpacked(&staged.inspected.digest),
            data_dir,
            platform: self.platform,
            settings,
        };
        assets::ensure_assets(&context, self.fetch)?;
        let touched = footprint::touched(&context)?;
        footprint::snapshot(&self.store.footprint_dir(&staged.cartridge), &touched)?;
        run_step(
            &context,
            StepKind::Setup,
            &self.log_dir,
            crate::constants::CARTRIDGE_SETUP_TIMEOUT,
        )
    }

    pub fn install(
        self,
        cart: &Path,
        settings: Option<Map<String, Value>>,
    ) -> crate::error::Result<Installed> {
        let staged = self
            .store
            .stage(cart, &self.cabinet, self.platform, self.now)?;
        let settings = match effective_settings(self.store, &staged, settings) {
            Ok(settings) => settings,
            Err(err) => {
                self.store.discard(&staged);
                return Err(err);
            }
        };
        let first = self
            .store
            .registry()
            .entry(&staged.cartridge)
            .is_none_or(|entry| entry.versions.is_empty());
        if !staged.held {
            if let Err(err) = self.prepare(&staged, &settings) {
                log::warn!("install of {} failed: {err}", staged.cartridge);
                if first {
                    rollback(self.store, &staged.cartridge);
                }
                self.store.discard(&staged);
                return Err(err);
            }
        }
        let installed = self.store.register(&staged, self.now, self.busy)?;
        self.store.update(&installed.cartridge, |entry| {
            entry.settings = settings;
            Ok(())
        })?;
        log::info!(
            "installed {} {} ({})",
            installed.cartridge,
            installed.version,
            installed.digest
        );
        Ok(installed)
    }
}

fn rollback(store: &Store, cartridge: &CartridgeRef) {
    let footprint = store.footprint_dir(cartridge);
    if let Err(err) = footprint::restore(&footprint) {
        log::warn!("cannot restore the footprint of {cartridge}: {err}");
    }
    let _ = footprint::forget(&footprint);
    let _ = cabinet_cartridge::archive::remove_tree(&store.data_dir(cartridge));
}

pub fn uninstall(
    store: &mut Store,
    cartridge: &CartridgeRef,
    platform: Platform,
    log_dir: &Path,
) -> crate::error::Result<Vec<String>> {
    let entry = store.entry(cartridge)?.clone();
    let mut problems = Vec::new();
    if let Some(digest) = entry.active.as_deref() {
        match store.manifest(digest) {
            Ok(manifest) => {
                if let Err(err) = run_uninstall_step(
                    store,
                    cartridge,
                    &manifest,
                    digest,
                    &entry.settings,
                    platform,
                    log_dir,
                ) {
                    problems.push(err.to_string());
                }
            }
            Err(err) => problems.push(err.to_string()),
        }
    }
    let footprint = store.footprint_dir(cartridge);
    problems.extend(footprint::restore(&footprint)?);
    store.remove_entry(cartridge)?;
    footprint::forget(&footprint)?;
    for problem in &problems {
        log::warn!("uninstall of {cartridge}: {problem}");
    }
    log::info!("uninstalled {cartridge}");
    Ok(problems)
}

fn run_uninstall_step(
    store: &Store,
    cartridge: &CartridgeRef,
    manifest: &Manifest,
    digest: &str,
    settings: &Map<String, Value>,
    platform: Platform,
    log_dir: &Path,
) -> crate::error::Result<()> {
    let context = Context {
        manifest,
        cartridge_dir: store.unpacked(digest),
        data_dir: store.data_dir(cartridge),
        platform,
        settings,
    };
    std::fs::create_dir_all(&context.data_dir)?;
    run_step(
        &context,
        StepKind::Uninstall,
        log_dir,
        crate::constants::CARTRIDGE_SETUP_TIMEOUT,
    )
}

#[cfg(test)]
mod tests;
