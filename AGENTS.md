# AGENTS.md

Guidance for agents working in this repository.

## About this project

Tooling for playing games with friends over a **Tailscale** tailnet, bypassing ISP CGNAT by keeping the traffic peer-to-peer. The app is a cross-platform Tauri app named **The Cabinet** (slug `the-cabinet`), providing session coordination, connection health, and low-bandwidth input-relay spectating. Stack: Tauri v2 (Rust backend) + Vite/React/Tailwind/shadcn frontend (the RetroArch core-download path uses `ureq` + `sha2`). **The RetroArch host/client/spectator launcher, connection health, macOS Cabinet mode, and the GitHub Actions → GitHub Releases distribution pipeline are implemented.** The app is being re-scoped into a general multi-game session coordinator — three transport drivers (`native_online`, `netplay`, `stream`) and declarative per-game profiles — per [`docs/design-general-emulation.md`](docs/design-general-emulation.md); the FightCade provider and launchers were **removed** (see [`docs/12-fightcade-ripout.md`](docs/12-fightcade-ripout.md)). The rooms/lobbies/KotH subsystem, the local lifetime score ledger, and the emulator result watcher were **removed on 2026-09-21** (see `docs/04-design.md` §3/§5); a replacement **lobby** (RetroArch-only rooms, sets, automatic rotation) is designed in `docs/09-lobby.md`; its **local build gate (spike S1–S5) passed 2026-09-21** and the **sets/rotation layer was validated live on loopback 2026-09-22** (a queue-preservation bug in the netplay tracker surfaced and was fixed; result counting is now gated on the game's round phase and both seats' control bytes, so arcade/CPU rounds, the arcade drift, and attract demos no longer count; timeouts hang off the round-end transition because the round counter never moves in versus play; the set boundary is the incoming player's own coin-in (the game's challenger flow), so no forced reset is needed; and a three-player rotation run fixed the seat-handover race — the incoming now waits for the losing seat to free before claiming it — plus the queue order, so a rotated-out loser re-enters at the back, and the room now publishes the coin the incoming still owes (`awaitingCoin`), which `RoomView` prompts them for — logged in `docs/10-lobby-spike.md` §6 L12–L20), while the online release gate (S6–S7: tailnet soak, beacon) in `docs/10-lobby-spike.md` is still pending. The **lobby core and sets/rotation are built** — the RetroArch command-socket plumbing, per-ROM result detection, the room model + HTTP beacon, hosting (with a host-seat choice), the seat/role split (the keyboard preset binds `input_player1_*`; the host seat is requested via `netplay_request_device_p1`/`p2`), the client join flow (`lobby_*` commands, `LobbyCard.tsx`), and `RoomView.tsx`, plus an FBNeo **SOCD preset** (default Simultaneous Neutral, written per session through a generated core-options file since RetroArch ignores `--appendconfig` for core options); the score/history ledger is not, and the game-boundary `RESET` (spike S9) is superseded — the boundary is the incoming player's own coin-in. Distribution uses `.github/workflows/ci.yml` (3-OS test/clippy gate) and `release.yml` (per-OS build → draft GitHub Release via `tauri-action`); tag `v*` to cut a release. Releases carry the `tauri-plugin-updater` payloads (`.app.tar.gz`/AppImage/NSIS `-setup.exe`) plus `latest.json` (the `.sig` files are build input for it, not uploaded), so the app updates itself in place from the published feed — Minisign-signed by a key held outside the repo (`~/.tauri/the-cabinet-updater.key`) and in GitHub Secrets; local `tauri build` needs it too. The frozen RetroArch cores ship separately as assets of a published `retroarch-cores-v1` release (uploaded by `scripts/publish-cores.sh`; the app downloads them in Settings → RetroArch), not with the app installers. The repo is **public** after the 2026-09-19 publication scrub (see `docs/04-design.md` §3/§8). Phase 0/0b, 3, 4 remain.

## Important: this repository is public

**`Tunmiseoni/the-cabinet` is a public GitHub repository** (since 2026-09-19, after a full-history scrub; see `docs/04-design.md` §3/§8). Everything committed, including full history, is world-readable. Before writing or committing anything, check it for leaks:

- No secrets or credentials of any kind (tokens, API keys, signing/updater keys, passwords). Put those in **GitHub Secrets, never in the repo**.
- No real network identifiers: tailnet IPs/hostnames, public IPs, LAN IPs, router/admin addresses, MagicDNS names, or account handles. Use only these sanctioned placeholders:
  - Tailnet: `100.64.0.1` (this Mac), `100.64.0.2` (CachyOS peer), `100.64.0.3` (Windows peer), or the generic `100.x.x.x`
  - LAN/router: `192.0.2.1`, `192.0.2.100`, `192.0.2.101`
  - ISP private hops: `198.51.100.2`–`198.51.100.4`
  - Public/STUN (TEST-NET-3): `203.0.113.x`
  - MagicDNS suffix: `example-tailnet.ts.net`
  - Host aliases: `mac-host`, `cachyos-host`, `windows-host`
  - Account handle: `player-one` (and `me@`/`friend@`)
- No ROMs, emulator binaries, `.env*`, or local machine paths/config with personal data.
- Code and docs are fine — `sfiii3nr1` and other public MAME identifiers are kept.

Secret scanning and push protection are enabled on GitHub, so a detected secret will be blocked on push (and if one ever lands, rotate it immediately — scrubbing history is not enough). Do not change repository visibility without the user's explicit request.

## Important: the user uses speech-to-text

The user dictates prompts via speech-to-text transcription. Expect and tolerate:

- Homophones and near-miss words (e.g. "retro arc" = RetroArch, "guilty gear's Drive" = Guilty Gear Strive, "lan scripts" = LAN scripts).
- Dropped or repeated words, run-on sentences, and inconsistent casing/punctuation.
- Product names spelled differently across messages.

**Do not silently guess when a term is ambiguous.** If a mis-transcription would change the technical decision, state the interpretation you are using and ask for confirmation. Prefer confirming intent over building on a guessed meaning.

## Changes to the host machine (always ask first)

Downloads, installs, and large or long-term changes to the host machine are **allowed, but never without asking first**. This is a consent-and-awareness requirement, not a hard boundary — it is not meant to stop the work, only to make sure the user knows about it.

Ask before doing any of these:

- Downloading or installing software, toolchains, or packages (Rust, Tauri, system libraries, Homebrew/npm/cargo installs).
- Changes that persist beyond the session (services, launch agents, scheduled jobs, shell profile / `PATH` edits, mounted drives).
- Modifying files outside this repository (e.g. installed emulators, system config).
- Anything large in size, network-heavy, or slow to reverse.

State what you intend to do, why, and how to undo it, then wait for confirmation. Prefer reversible, repo-local changes; when a global change is genuinely needed, say so explicitly.

## Repository layout

- `docs/` — investigation and design. `01`–`03` are the historical troubleshooting record; `04-design.md` is the FightCade-era spec (its scope is superseded); [`design-general-emulation.md`](docs/design-general-emulation.md) is the agreed re-scope and [`12-fightcade-ripout.md`](docs/12-fightcade-ripout.md) its first step; `08-cleanup.md` is the cleanup backlog (applied pass + deferred refactors); `11-training-mode.md` is research-only (two training paths: Fightcade/peon2 + `3rd_training_lua` via Lua, licensing-gated; and a Cabinet-driven RetroArch FBNeo practice mode over the command socket, added 2026-09-22 — both gated on spikes + a `/grill-me` session).
- `frontend/` — Vite + React + TypeScript + Tailwind v4 + shadcn/ui (alias `@/*` -> `frontend/src/*`).
- `src-tauri/` — Rust backend (`config`, `net` (the network provider; Tailscale is the first, over the `tailscale` parsers), `roms`, `contracts`, `providers/retroarch/{core,hotkeys,parity,spec,command}`, `lobby`, `session`, `supervisor` (process trees), `tasks` (the per-session cancellable task tree), `windowing`, `commands`).
- `scripts/` — helper scripts (`dev.sh`, `build.sh`, `test.sh`, `clean.sh`, `setup-linux.sh`, `uninstall-linux.sh`, `diagnose-linux.sh`, `tauri-build.sh`, `patch-appimage.sh`, `retroarch-spike.sh`, `ram-probe.py`, `ram-window.py`, `publish-cores.sh`). `publish-cores.sh` uploads the frozen cores as assets of a published `retroarch-cores-v1` release on the public repo (never the repo tree); run it only on the user's say-so.
- The Tauri app is the implementation.
- **Temporary workaround — remove when upstream lands:** `scripts/patch-appimage.sh` strips the over-bundled Wayland/X11 client libraries from the Linux AppImage to fix the `EGL_BAD_PARAMETER` blank window on Mesa 25+ hosts (tauri-apps/tauri#15976). It runs from `scripts/tauri-build.sh` (the `tauriScript` used by `release.yml`) and `scripts/build.sh`, and no-ops off Linux or when those libraries are already absent. **To remove it:** once Tauri supports `bundle.linux.appimage.excludeLibraries` (PR #15662), set that in `tauri.conf.json` and delete `scripts/patch-appimage.sh`; the guarded call sites become no-ops. Full steps are in the script header and `docs/04-design.md` §3.
- **Linux host-tool subprocesses:** `src-tauri/src/process.rs` strips `LD_LIBRARY_PATH`/`LD_PRELOAD` from every spawned host tool (`tailscale`, the emulator, future streamers). The AppImage's AppRun prepends its bundled libraries to `LD_LIBRARY_PATH`; leaving it set made a host tool load the bundle's glib against the system libraries and exit non-zero. Keep the strip for any new host-tool spawn. Long-running programs go through `supervisor::spawn` (built on `process::command`), which owns the process tree: its own process group on macOS/Linux, a kill-on-close Job Object on Windows; background work for a session runs as tasks in the session's `TaskTree`, not detached threads. Network identity and addresses come from `net::provider`, never from `tailscale` directly; the room beacon binds the provider's tailnet address plus `127.0.0.1`, not `0.0.0.0`.

## Environment facts (observed)

- Primary machine: Apple M1 (arm64), 8 GB RAM, macOS 26.x.
- Emulator: RetroArch (native), with the frozen FBNeo core downloaded in-app into `<app_data_dir>/cores/<platform>/`.
- Tailnet is currently **direct, <100 ms** between peers after a router restart (earlier DERP-relay notes are stale; see amended `01`/`02`).
- Other machines: a CachyOS friend and a Windows friend, both on the tailnet.

## Conventions

- Keep documentation in `docs/`; do not create new top-level notes.
- Prefer editing existing docs over creating new ones, except where `04-design.md` is the designated spec.
- Do not add comments to code unless asked.
- Match the existing shell-script style and the docs' use of tables.
- Log through the `log` crate (`log::info!`/`warn!`/`error!`/`debug!`), which `tauri-plugin-log` routes to `app_log_dir`; do not use `eprintln!`/`println!` outside `#[cfg(test)]`.
- Lock mutexes with `sync::MutexExt::lock_or_recover` (recovers from a poisoned lock) rather than `.lock().unwrap()`.
- Shared magic values (timeouts, intervals) live in `constants.rs`; time helpers in `time.rs`.
- Fallible functions return `crate::error::Result<T>` (`CabinetError`), so `?` works across modules; convert to `String` only at the `#[tauri::command]` boundary via `crate::error::CommandResult<T>` (`CommandError` serializes its `Display`). Use `CabinetError::Message`/`From<String>` for user-facing text; `home_dir`/PATH probes live in `env.rs`.

## Environment setup

Rust via the official installer (not Homebrew's keg-only `rustup`, which conflicts with the `rust` formula):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Node/npm (already present via nvm) runs the Vite frontend. The Tauri CLI is a root `package.json` devDependency (`@tauri-apps/cli`), **not** a global `cargo install`.

```sh
npm install                      # root: Tauri CLI
npm install --prefix frontend    # frontend: React/Vite/Tailwind/shadcn
npm run tauri dev                # Vite on :1420 + Rust backend
```

Project layout: `frontend/` (Vite + React + TS + Tailwind v4 + shadcn/ui; alias `@/*` -> `frontend/src/*`) and `src-tauri/` (Rust backend). Per-OS system dependencies: see the Prerequisites subsection in [`docs/04-design.md`](docs/04-design.md).

## Testing

- There is no automated test suite yet.
- Frontend typecheck/build: `npm run build` (root, delegates to `frontend/`). Rust checks in `src-tauri/`: `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets`. The same gate runs on all three OSes in `.github/workflows/ci.yml`.
- Rust unit tests cover the Tailscale parsers, ROM scan, and the RetroArch launch spec. Opt-in live tests require local hardware: `cargo test -- --ignored --nocapture` (Tailscale status/ping, ROM dir, the host RetroArch core-options lookup, real RetroArch launch and loopback pair).
- Direct-connect can be tested on a single machine over loopback: the app's **Dev pair** button (or `launch_dev_pair`), or two player instances.
- For manual end-to-end tests, at least two peers must be online on the tailnet.
- Helper scripts: `scripts/test.sh` runs the full local gate (frontend build + `cargo fmt --check` + `cargo test` + clippy); `scripts/dev.sh` runs the app; `scripts/clean.sh` removes regenerables (`--deps`).
- `.github/workflows/smoke.yml` is a manual (`workflow_dispatch`) smoke-launch: it builds each OS with `--debug --no-bundle`, starts the app under a virtual display, and uploads a screenshot. It is not called by `ci.yml` or `release.yml`.

## Git

- The repository is **`Tunmiseoni/the-cabinet`**, **public** since 2026-09-19 (see "this repository is public" above). Its full history was scrubbed of tailnet/public/LAN addresses and account handles (see `docs/04-design.md` §8, the completed Publication scrub).
- **Distribution is GitHub Actions → GitHub Releases** (`docs/04-design.md` §3/§7). Build workflows belong in `.github/workflows/`. Any signing/updater keys or tokens go in **GitHub Secrets, never in the repo**.
- Keep the scrub intact. Do not reintroduce real addresses/handles — prefer placeholders (see `docs/04-design.md` §8).
- When committing: commit `Cargo.lock` (this is an application, not a library); never commit `target/`, `node_modules/`, emulator binaries, ROMs, or `.env*`.
