//! winapk — CLI shell (replaces the Electron orchestrator UI).
//! Double-clicked .apk lands here via the file association → `install`.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use winapk_core::paths::Paths;
use winapk_core::{err, info, warn, Config};
use winapk_engine::Engine;
use winapk_install::store::Store;

#[derive(Parser)]
#[command(name = "winapk", version, about = "Android apps as native Windows windows")]
struct Cli {
    /// Verbose logging (DBG lines to stderr+log file).
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download/install the emulator runtime + system image (~1.2 GB first time).
    Provision,
    /// Install an APK: parse → adb install → AOT speed → desktop shortcut.
    Install {
        apk: PathBuf,
        /// Skip desktop shortcut creation.
        #[arg(long)]
        no_shortcut: bool,
    },
    /// Run an installed package in its window (boots the VM if needed).
    Run { pkg: String },
    /// List installed apps (from store.json).
    List,
    /// Stop the VM gracefully.
    Stop,
    /// Environment diagnostics (accel, GPU, cores, runtime status).
    Doctor,
    /// Register/unregister the .apk file association (HKCU, no admin).
    Assoc { action: String },
    /// Sync the shared folder ↔ /sdcard/WINapkShare.
    Share { direction: String },
    /// Get/set config values (cores, ram_mb, userdata_gb, gpu_mode, ...).
    Config {
        key: Option<String>,
        value: Option<String>,
    },
    /// Inspect an APK without installing (manifest, label, icon, ABIs).
    Inspect { apk: PathBuf },
}

fn open_engine() -> winapk_core::Result<(Paths, Config, Engine)> {
    let paths = Paths::new()?;
    paths.ensure_all()?;
    let config = Config::load(&paths.config_file())?;
    let engine = Engine::new(paths.clone(), config.clone());
    Ok((paths, config, engine))
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            err!("{e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> winapk_core::Result<()> {
    let (paths, config, mut engine) = open_engine()?;
    winapk_core::log::init(
        Some(&paths.logs_dir().join("winapk.log")),
        cli.verbose,
    );

    match cli.cmd {
        Cmd::Provision => {
            engine.ensure_provisioned(|stage, _d, _t| {
                info!("provision: {stage}");
            })?;
            info!("runtime ready at {}", paths.runtime().display());
        }

        Cmd::Doctor => {
            for l in engine.doctor() {
                println!("{l}");
            }
        }

        Cmd::Inspect { apk } => {
            let info = winapk_install::apk::analyze(&apk)?;
            println!("package : {}", info.package);
            println!("label   : {}", info.label);
            println!(
                "version : {} ({})",
                info.version_name.as_deref().unwrap_or("?"),
                info.version_code.unwrap_or(0)
            );
            println!("launcher: {}", info.launcher.as_deref().unwrap_or("?"));
            println!("abis    : {:?}", info.abis);
            println!("icon    : {}", info.icon_entry.as_deref().unwrap_or("(none/fallback)"));
            println!("min_sdk : {}", info.min_sdk.unwrap_or(0));
        }

        Cmd::Install { apk, no_shortcut } => {
            engine.ensure_running()?;
            engine.bind_adb()?;
            let adb = engine.adb().cloned().unwrap();
            let exe = std::env::current_exe()?;
            let rep = winapk_install::install_apk(
                &apk,
                &adb,
                &paths,
                &exe,
                !no_shortcut,
            )?;
            println!(
                "installed {} ({}){}",
                rep.info.package,
                rep.info.label,
                rep.shortcut
                    .map(|p| format!(" — shortcut {}", p.display()))
                    .unwrap_or_default()
            );
            if !rep.aot_done {
                warn!("AOT compile skipped/failed — app will JIT-warm instead");
            }
        }

        Cmd::Run { pkg } => {
            engine.run_app(&pkg)?;
            let adb = engine.adb().cloned().or_else(|| {
                engine.bind_adb().ok().and_then(|_| engine.adb().cloned())
            });
            let Some(adb) = adb else {
                return Err(winapk_core::Error::Emulator("no adb".into()));
            };

            // Resolve label/icon for the window chrome.
            let store = Store::load(&paths.store_file());
            let rec = store.find(&pkg);
            let label = rec.map(|r| r.label.clone()).unwrap_or_else(|| pkg.clone());
            let ico = rec.map(|r| PathBuf::from(r.icon.clone()));
            let crop = if config.window.mode == "per-app" {
                // Give the freeform window a moment to lay out, then read bounds.
                std::thread::sleep(Duration::from_secs(2));
                adb.freeform_bounds(&pkg)
            } else {
                None
            };

            let emu_pid = engine.emulator_pid().or_else(|| {
                std::fs::read_to_string(paths.root().join("emulator.pid"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
            });

            let req = winapk_display::DisplayRequest {
                emulator_pid: emu_pid.unwrap_or(0),
                title: label.clone(),
                ico,
                crop,
                width: config.window.portrait_w,
                height: config.window.portrait_h,
            };
            match winapk_display::attach(&paths, &req, &pkg)? {
                Some(mut host) => {
                    info!("window attached — close it to end the session");
                    host.wait(None);
                    engine.app_closed(&pkg)?;
                }
                None => {
                    info!("app started (no display host; emulator window standalone)");
                }
            }
        }

        Cmd::List => {
            let store = Store::load(&paths.store_file());
            if store.apps.is_empty() {
                println!("no apps installed yet — `winapk install <apk>`");
            }
            for a in &store.apps {
                println!(
                    "{:40} {:50} {}{}",
                    a.pkg,
                    a.label,
                    a.version_name.clone().unwrap_or_default(),
                    if a.aot_done { " [AOT]" } else { "" }
                );
            }
        }

        Cmd::Stop => {
            engine.stop()?;
            info!("VM stopped");
        }

        Cmd::Assoc { action } => {
            let exe = std::env::current_exe()?;
            match action.as_str() {
                "on" => {
                    winapk_install::shortcut::assoc_on(&exe)?;
                    info!(".apk association registered → {}", exe.display());
                }
                "off" => {
                    winapk_install::shortcut::assoc_off()?;
                    info!(".apk association removed");
                }
                _ => warn!("usage: winapk assoc on|off"),
            }
        }

        Cmd::Share { direction } => {
            engine.ensure_running()?;
            engine.bind_adb()?;
            let adb = engine.adb().cloned().unwrap();
            match direction.as_str() {
                "push" => {
                    let n = winapk_share::push(&adb, &paths)?;
                    info!("pushed {n} file(s)");
                }
                "pull" => {
                    let n = winapk_share::pull(&adb, &paths)?;
                    info!("pulled {n} file(s)");
                }
                _ => warn!("usage: winapk share push|pull"),
            }
        }

        Cmd::Config { key, value } => {
            match (key, value) {
                (None, _) => {
                    println!("{}", toml::to_string_pretty(&config).unwrap());
                }
                (Some(k), None) => {
                    let txt = toml::to_string_pretty(&config).unwrap();
                    for l in txt.lines() {
                        if l.starts_with(&k) {
                            println!("{l}");
                        }
                    }
                }
                (Some(k), Some(v)) => {
                    let mut cfg = engine.config.clone();
                    set_config(&mut cfg, &k, &v)?;
                    cfg.save(&paths.config_file())?;
                    engine.config = cfg;
                    info!("config {k} = {v}");
                }
            }
        }
    }
    Ok(())
}

fn set_config(c: &mut Config, key: &str, val: &str) -> winapk_core::Result<()> {
    use winapk_core::config::GpuMode;
    let e = &mut c.engine;
    let num = |s: &str| s.parse::<u32>();
    match key {
        "cores" => e.cores = num(val).unwrap_or(0),
        "ram_mb" => e.ram_mb = num(val).unwrap_or(0),
        "userdata_gb" => e.userdata_gb = num(val).unwrap_or(64),
        "snapshot" => e.snapshot = val == "true" || val == "1",
        "pin_p_cores" => e.pin_p_cores = val == "true" || val == "1",
        "no_ecoqos" => e.no_ecoqos = val == "true" || val == "1",
        "virtio_fs" => e.virtio_fs = val == "true" || val == "1",
        "gpu_mode" => {
            e.gpu_mode = match val {
                "host" => GpuMode::Host,
                "angle_indirect" | "angle" => GpuMode::AngleIndirect,
                "swiftshader" | "software" => GpuMode::Swiftshader,
                _ => GpuMode::Host,
            }
        }
        _ => warn!("unknown key {key} (cores|ram_mb|userdata_gb|gpu_mode|snapshot|pin_p_cores|no_ecoqos|virtio_fs)"),
    }
    Ok(())
}
