# AGENT.md — maintaining openc3-cosmos-app

Guidance for an AI agent (or new human maintainer) working on this repo. It
captures architecture, conventions, and the non-obvious lessons that are easy to
regress.

## What this is

A **Rust native launcher/manager for OpenC3 COSMOS**. Two faces from one binary:

- a **GUI** (iced 0.13, the `tiny-skia` *software* renderer — no GPU) for setup,
  starting/stopping COSMOS, container status, logs, and the bridge; and
- a **CLI** (`clap`) with subcommands (`run`, `stop`, `cleanup`, `cli`, `util`,
  `microservices`, `bridgeenroll`, …).

Its headline feature is the **Iroh bridge**: it runs COSMOS interfaces that need
host hardware (serial ports, USB, etc.) *outside* Docker on the host, tunneling
their bytes to COSMOS running in containers.

## Build / test / lint

Default features include `gui`. Build with `--no-default-features` for a smaller
headless CLI-only binary.

```bash
cargo build --features gui           # GUI build (default)
cargo build --no-default-features    # headless CLI only (no iced/rfd/image/fs4)
cargo clippy --features gui          # lint GUI; treat warnings as errors mentally
cargo clippy                         # default == gui feature on
cargo test --features gui            # unit tests
```

- `gui = ["dep:iced", "dep:rfd", "dep:image", "dep:fs4"]`. Many functions are
  gated `#[cfg_attr(not(feature = "gui"), allow(dead_code))]` because they're
  only called from the GUI — keep that annotation when adding GUI-only helpers,
  or the headless build warns.
- **Keep clippy clean on both feature sets.** A change that's clean under `gui`
  can warn under `--no-default-features` (e.g. a helper that becomes unused), and
  vice-versa. Also check platform-gating: a fn used only under
  `#[cfg(target_os = "linux")]` will look dead on macOS (see
  `process::stdout_string`, annotated `#[cfg_attr(not(target_os="linux"), allow(dead_code))]`).

## ⚠️ Cross-repo dependency (read this first)

The **Python half of the bridge lives in the OpenC3 COSMOS core repo**, NOT here:

- `openc3/python/openc3/microservices/bridge_microservice.py` — the Iroh hub.
- `openc3/python/openc3/microservices/host_interface_microservice.py` — the
  host-side runner this app spawns (opens the real device).
- `openc3/python/openc3/interfaces/bridge_interface.py` — the COSMOS-side
  `Interface` that tunnels raw bytes.
- `openc3/python/openc3/models/host_microservice_model.py` and
  `openc3/lib/openc3/models/interface_model.rb` (Ruby) — the `BRIDGE`/
  `BRIDGE_OPTION`/`BRIDGE_PROTOCOL` config and the `HostMicroserviceModel` the
  app polls over `api/host_microservices`.

**Any change to the bridge wire protocol (ALPNs, PRIME/READY/GO handshake,
control-channel JSON, the host-microservices/interface-status APIs) must be made
in lockstep across this repo and the COSMOS core repo.** They were developed
together; the Rust side here is only the middle relay + host-process supervisor.

## Module map (`src/`)

- `main.rs` — entry; CLI dispatch; `windows_subsystem="windows"`; `ensure_tool_path()`.
- `cli.rs` — clap command definitions.
- `commands.rs` — CLI command implementations (`run`, `stop`, `cleanup`, …).
- `context.rs` — `Context`/`Paths` (root resolution, runtime detection, docker
  engine installed/running checks).
- `docker.rs` — `docker compose` invocation + the Linux docker-group re-exec (`sg docker`).
- `dockerapi.rs` — Docker Engine API over the socket/pipe via **bollard** (status + stats).
- `monitor.rs` — container status snapshot (bollard, CLI fallback), health, totals.
- `install.rs` — installers (Docker, Python/uv, COSMOS env), Windows features,
  virtualization check, the notifier/dialog/progress plumbing.
- `enroll.rs` — the app's Iroh identity + bridge enrollment (auto via local
  Docker `bridgeenroll`, or a manual token). Caches the hub ticket.
- `bridge.rs` — `BridgeClient` (talks to the hub: files, log, authorize,
  interface-status APIs).
- `operator.rs` — supervises host-interface microservices; drives bridge
  auto-enroll; publishes status to the GUI.
- `hostfiles.rs` — mirrors scope plugin files to the host so host interfaces can
  import custom code; fingerprints venv inputs.
- `gui.rs` — the entire iced app (pages: Splash/Install/Main, modals, tray).
- `tray.rs` — system-tray/menu-bar icon + Dock-visibility (macOS).
- `single_instance.rs` — single-instance guard (file lock) + "show existing window".
- `settings.rs` — persisted settings (`openc3-cosmos-app-settings.json`).
- `process.rs` — subprocess helpers (`run`, `capture`, `run_streamed`, `no_window`).
- `download.rs` — curl/wget wrappers.
- `env_file.rs`, `logging.rs`, `util.rs` — support.

## The bridge (data flow)

```
device  <->  host_interface_microservice (host, spawned by this app's operator)
        <--host/<name>-->  bridge_microservice (hub, in COSMOS Docker)
        <--stream/<name>-->  bridge_interface (COSMOS Interface, in Docker)
```

- **ALPNs**: `stream/<name>` (COSMOS data leg), `host/<name>` (host data leg),
  `ctrl/<name>` + `hostctrl/<name>` (control), `api/*` (control APIs:
  host_microservices, log, authorize, enroll, files, interface_status).
- **Pairing**: the hub rendezvous-pairs the two legs by name. It opens+primes
  each bi-stream with a 1-byte `PRIME` immediately on arrival (so `accept_bi()`
  returning is *not* proof the peer is present — see handshake).
- **READY/GO handshake** (in `bridge_interface.py` / `host_interface_microservice.py`):
  after pairing, the host sends `READY` (it's up), COSMOS replies `GO`; only then
  does the host open the device. This enforces: COSMOS won't report "connected"
  until the host is ready, and the host won't touch hardware until COSMOS is up.
- **Control channel**: COSMOS sends connect/disconnect (coalesced on the host to
  the net final state to avoid replaying a backlog); the host pushes live
  `InterfaceStatus` up, surfaced in CmdTlmServer and openc3-cosmos-app.
- **BRIDGE_PROTOCOL**: protocols declared `BRIDGE_PROTOCOL` run on the *host*
  (next to the device); plain `PROTOCOL`s run in COSMOS on `bridge_interface`.
- Host errors → host disconnects and **parks** (does not auto-reconnect); COSMOS
  is authoritative and re-drives the connection.

## Platform behavior & hard-won lessons

### Testing caveat (important)
This project's primary dev machine is **macOS with Homebrew-managed Rust (no
`rustup`)**, so you **cannot cross-compile the Windows target here**. macOS code
paths (incl. `objc2`/tray) are compile/run-testable; **Windows and Linux paths
are review-verified only** — call that out and lean on the standard APIs.

### Windows (`windows_subsystem = "windows"` → no console)
- **Every subprocess must be windowless.** Route through `process::{run,
  capture, run_streamed}` (they call `no_window()`), or call
  `process::no_window(&mut cmd)` before spawn. A bare `Command` flashes a console
  window — this bit `probe_cosmos` (a polled `curl`).
- **Exe icon**: `build.rs` embeds `assets/icons/icon.ico` via `winresource`
  (a Windows-host build-dep) so Explorer/taskbar/shortcuts show the logo.
- **Static CRT**: `.cargo/config.toml` sets `+crt-static` so the exe doesn't need
  `VCRUNTIME140.dll`.
- **PATH**: `ensure_tool_path()` (main.rs) prepends Docker Desktop's `resources\bin`
  to PATH at startup *unconditionally* — Docker's system-PATH update isn't seen by
  an already-running process, so first-run detection would otherwise fail until
  restart.
- **Optional features**: checked via the `Win32_OptionalFeature` WMI class
  (readable without admin — no UAC), enabled via an elevated PowerShell
  (`Start-Process -Verb RunAs`) that runs `Enable-WindowsOptionalFeature`. A
  restart prompt follows. **BIOS virtualization** is detected via
  `HypervisorPresent` / `VirtualizationFirmwareEnabled` (detect-and-inform only —
  can't be enabled in software).
- `wsl --update` runs before starting Docker.
- Installer is **WiX `.msi` only**; code-signed via **Azure Trusted Signing**
  (SmartScreen). Product name `OpenC3_COSMOS` (no spaces).

### macOS
- Docker Desktop installs from the **official DMG** (not Homebrew — brew's cask
  shells out to `sudo` with no TTY and trips on existing credential helpers).
- **Tray + Dock**: hiding to the tray switches the app to
  `NSApplicationActivationPolicy::Accessory` (no Dock icon) via `objc2`; showing
  restores `Regular`. **GOTCHA:** `setActivationPolicy:` returns `BOOL`, so the
  `msg_send!` must declare a `bool` return — declaring `()` aborts at runtime with
  "expected 'B', found 'v'" (objc2 verifies encodings). See `tray.rs`.
- DMG is **notarized in CI** on the final artifact (cargo-packager's inline
  `notarytool submit --wait` is silent/unbounded and hung — do notarization
  ourselves with a timeout+staple).

### Linux
- **No tray** (`tray::ENABLED == cfg!(any(windows, macos))`). So the window
  close (X) shows a **"Quit?" confirmation modal** instead of hiding.
- **Docker group**: joining prefers **`pkexec`** (graphical polkit prompt — works
  with no terminal) over a TTY-less `sudo`; on failure it pops up the exact manual
  commands (`groupadd` / `usermod -aG docker $USER` / `newgrp docker`). In-session
  it re-execs via `sg docker` so Docker works without a re-login.

## Key subsystems & conventions

- **User messaging** (`install.rs`): `install::progress(msg)` / `notify(...)`
  routes to the GUI activity log (when a notifier is set) or stdout (CLI).
  `notify_dialog(msg)` additionally pops a dismissible modal in the GUI (used for
  post-install NEXT STEPS). Long-running output (e.g. `docker compose` image
  pulls) is streamed live via `process::run_streamed(cmd, |line| install::progress(line))`,
  and the GUI shows a spinner + latest line while `busy`.
- **Single instance** (`single_instance.rs`): an **advisory file lock** on
  `<root>/openc3-cosmos-app.lock` (chosen over a fixed TCP port — no collision, OS
  releases on crash). A second launch drops an `openc3-cosmos-app.show` marker and exits;
  the running app polls it and raises its window. Fail-open if locking errors.
- **Self-update** (`update.rs`, GUI-only): a background thread checks the GitHub
  Releases API (`DEFAULT_REPO`, overridable via `OPENC3_COSMOS_APP_GITHUB_REPO`) on
  startup and every 8h, storing the latest `Release` in a shared slot. The Main
  page's tick surfaces an **Update available** modal when the release is newer
  than `env!("CARGO_PKG_VERSION")` and isn't the `settings.skipped_version`.
  **Install** downloads the platform/arch installer asset and launches it
  (`open` dmg / `start` msi / `xdg-open` deb), falling back to opening the release
  page; **Skip this version** persists to settings; **Later** dismisses until the
  next check. The Settings page also has a **Check for updates now** button
  (`update::check_now` → `CheckOutcome`), which reports even when up-to-date/failed
  (via the dialog popup) and ignores the skip preference. Release tags are
  `v<semver>` — `strip_tag` normalizes them; keep `DEFAULT_REPO` in sync with the
  actual repo path.
- **COSMOS self-update** (`update.rs` + `commands::upgrade_cosmos`, GUI-only): a
  parallel background thread (`spawn_cosmos_checker`, startup + 8h) checks the
  **tags** of `cosmos-project` (Core, GitHub) or `cosmos-enterprise-project`
  (Enterprise, Forgejo `repos.openc3.com` — needs the enterprise token) via
  `cosmos_latest`, comparing the highest semver tag against the installed
  `OPENC3_TAG` read from `cosmos/.env` (`installed_cosmos_version`). No-op in dev
  mode or when COSMOS isn't installed. A newer, non-`cosmos_skipped_version`
  release surfaces a **COSMOS update available** modal on Main (one modal at a
  time — the app-update prompt takes precedence). **Update** runs
  `commands::upgrade_cosmos`: read `.env.local` + `compose.override.yaml` into
  memory → `stop` → `remove_dir_all(cosmos)` → `install::cosmos(tag)` (fresh) →
  restore those two files → `run`. Data safety: telemetry/config lives in Docker
  **volumes** (untouched by a folder replace); user config/secrets live in
  `.env.local`/`compose.override.yaml` (COSMOS 7.3.0+ convention, explicitly
  preserved). The **Check for updates now** button covers both app and COSMOS
  (`check_now(env_path, enterprise, token)` → `CheckOutcome::AppUpdate` |
  `CosmosUpdate` | `UpToDate` | `Failed`, app checked first).
- **Tray icon**: a purpose-built **"COS/MOS" badge** (`assets/tray.png`,
  generated by `tools/gen_tray_icon.py`). Do NOT use the downscaled app logo
  (blurry at 16px) or hand-drawn Unicode triangles (tofu on Windows). Disclosure
  carets in the Container-Status header come from the embedded icon font
  (`assets/openc3-icons.ttf`, `tools/gen_icons.py`) for the same reason.
- **iced performance**: the software renderer is very slow in **debug** builds —
  `[profile.dev.package."*"] opt-level = 3` in Cargo.toml is the real fix. Don't
  add per-frame fast-tick / poll-skip band-aids to "speed up" the UI.
- **Container status/stats**: via the Docker socket (bollard, `dockerapi.rs`);
  `monitor.rs` falls back to `docker compose ps`/`stats` if the socket is
  unreachable. Health: a one-shot container that **exited 0** (e.g.
  `openc3-cosmos-init`) counts as healthy — see `ContainerStatus::is_healthy`.
- **Settings** (`settings.rs`): `openc3-cosmos-app-settings.json` in the app root
  (gitignored — contains the enterprise access token; rotate if it leaks).
  Includes `cosmos_url`, `run_locally`, `edition`, `enterprise_token`, `dev_mode`,
  `dev_folder`.
- **Editions**: Core (github `cosmos-project`) vs Enterprise (private Forgejo at
  `repos.openc3.com`, needs an access token sent as an `Authorization: token`
  header).
- **Run-locally = false**: don't offer Docker/COSMOS installs or start/stop; only
  the host Python runtime is needed (for the bridge microservices); "Open in
  Browser" uses the configured COSMOS URL.
- **Development Mode**: sets `OPENC3_TAG`/`OPENC3_ENTERPRISE_TAG=latest` and
  `OPENC3_DEVEL` (editable openc3 install in host venvs), and uses a chosen dev
  folder's `openc3.sh`/`compose.yaml`. Toggling it **restarts the operator and
  re-enrolls the bridge** against the new compose context (clears an auto-enrolled
  ticket; a **manual** token is preserved). See `single_instance`-adjacent logic
  in `gui.rs` and `enroll::forget_cached_ticket`.

## Gitignored runtime state (never commit)

`target/`, `dist/`, `python/` (venv), `cosmos/` (COSMOS install), `bin/`,
`bridge/` (contains `identity.key` — a private key), `host_files/`,
`microservices/` (host-microservice working dirs), `openc3-cosmos-app-settings.json`,
`*.profile.json.gz`, `.DS_Store`. See `.gitignore`.

## CI / release

`.github/workflows/openc3-cosmos-app-release.yml` builds native installers on per-OS
runners (dmg on macOS, deb+appimage on Linux, WiX msi on Windows) plus a Linux
`.tar.gz`. `workflow_dispatch` builds artifacts only; pushing a **`v*`**
tag additionally attaches them to a GitHub Release. It was moved from the COSMOS
monorepo and de-monorepo'd (no `working-directory: openc3-cosmos-app`, artifact paths are
repo-root-relative). Required repo secrets for signed/notarized output (all
optional — absent → unsigned build):
- macOS: `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
  `APPLE_API_KEY`, `APPLE_API_ISSUER`, `APPLE_API_KEY_P8`.
- Windows: `AZURE_TS_ENDPOINT`, `AZURE_TS_ACCOUNT`, `AZURE_TS_PROFILE`,
  `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`.

## Follow-ups / not yet in this repo

- No `remote` is configured yet — push when ready.
- Several bridge/GUI features are **live-untested on Windows/Linux** (see the
  testing caveat); prefer verifying on real hardware before shipping.
