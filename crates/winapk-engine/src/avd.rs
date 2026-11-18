//! AVD authoring: writes WINapk.avd/config.ini + WINapk.ini pointer.
//! The profile feeds hw.* keys directly — the same values upstream clamps
//! (2–4 cores / 2–4 GB / 4 GB userdata) become configurable, with qcow2
//! userdata defaulting to a 64 GiB sparse ceiling.

use std::fs;
use std::path::Path;

use winapk_core::config::EngineProfile;
use winapk_core::error::{Error, Result};
use winapk_core::paths::Paths;

pub const AVD_NAME: &str = "WINapk";

/// (Re)write the AVD. `sysimg` = extracted google_apis/x86_64 dir
/// (userdebug — adbd trusts seeded keys, no auth dialog; Play Store
/// flavour is a `user` build that dead-ends headless automation).
pub fn create(paths: &Paths, profile: &EngineProfile, sysimg: &Path) -> Result<()> {
    let dir = paths.avd_dir();
    fs::create_dir_all(&dir)?;

    let ram = profile.resolved_ram_mb(host_total_mb());
    let cores = profile.resolved_cores(host_logical_cpus());

    let ini = format!(
        "avd.ini.encoding=UTF-8\n\
         path={}\n\
         path.rel=avd/WINapk.avd\n\
         target=android-34\n",
        dir.to_string_lossy().replace('\\', "/")
    );
    fs::write(paths.avd_ini(), ini)?;

    let mut cfg = String::new();
    let mut put = |k: &str, v: &str| {
        cfg.push_str(k);
        cfg.push('=');
        cfg.push_str(v);
        cfg.push('\n');
    };

    put("avd.ini.displayname", "WINapk");
    put("avd.ini.encoding", "UTF-8");
    put("target", "android-34");
    put("path.rel", "avd/WINapk.avd");
    put("path", &dir.to_string_lossy().replace('\\', "/"));
    put("AvdId", "WINapk");
    put("abi.type", "x86_64");
    put("hw.cpu.arch", "x86_64"); // without this, QEMU2 assumes 'arm' and dies
    put("image.sysdir.1", &sysimg.to_string_lossy().replace('\\', "/"));
    put("tag.id", "google_apis");
    put("tag.display", "Google APIs");

    // CPU / RAM — the profile's real values, not upstream's clamps.
    put("hw.cpu.ncore", &cores.to_string());
    put("hw.ramSize", &ram.to_string());
    put("vm.heapSize", &heap_for(ram).to_string());
    // Sparse userdata: qcow2 grows on demand up to this ceiling.
    put("disk.dataPartition.size", &format!("{}G", profile.userdata_gb));

    // Display.
    put("hw.lcd.width", &profile.lcd_width.to_string());
    put("hw.lcd.height", &profile.lcd_height.to_string());
    put("hw.lcd.density", &profile.lcd_dpi.to_string());
    put("hw.displayRegion.0.1.xOffset", "-1");
    put("hw.displayRegion.0.1.yOffset", "-1");
    put("hw.displayRegion.0.1.width", "0");
    put("hw.displayRegion.0.1.height", "0");

    // GPU passthrough — never "software" unless the user pinned it.
    put("hw.gpu.enabled", "yes");
    put("hw.gpu.mode", profile.gpu_mode.as_arg());

    // Sensible defaults for app use (vs dev-focused defaults).
    put("hw.keyboard", "yes");
    put("hw.mainKeys", "no"); // nav bar on screen
    put("ShowDeviceFrame", "no"); // no tablet bezel around the window
    // Orientation sensor OFF: enabled it auto-rotates the whole display and
    // wrecks freeform window layout (upstream battle damage).
    put("hw.sensors.orientation", "no");
    put("hw.accelerometer", "no");
    put("hw.audioInput", "yes");
    put("hw.audioOutput", "yes");
    put("hw.battery", "yes");
    put("hw.camera.back", "virtualscene");
    put("hw.camera.front", "emulated");
    put("hw.gps", "yes");
    put("hw.gsmModem", "yes");
    put("hw.sensors.light", "yes");
    put("hw.sensors.proximity", "yes");
    put("hw.trackBall", "no");
    put("hw.device.manufacturer", "Google");
    put("hw.device.name", "pixel_6");
    put("hw.wifi", "yes");
    put("hw.arc", "false");
    put("PlayStore.enabled", "no");
    put("root.present", "no");
    put("fastboot.chosenSnapshotFile", "");
    put("fastboot.forceChosenSnapshotBoot", "no");
    put("fastboot.forceColdBoot", "no");
    put("fastboot.forceFastBoot", "yes");
    put("runtime.network.latency", "none");
    put("runtime.network.speed", "full");
    // virtio-serial agent channel (chardev args built at launch time).
    put("hw.winapk.agentPort", &profile.agent_port.to_string());

    fs::write(dir.join("config.ini"), cfg)?;
    Ok(())
}

fn heap_for(ram_mb: u32) -> u32 {
    match ram_mb {
        0..=3072 => 512,
        3073..=6144 => 768,
        _ => 1024,
    }
}

pub fn host_logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

pub fn host_total_mb() -> u64 {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{
            GlobalMemoryStatusEx, MEMORYSTATUSEX,
        };
        unsafe {
            let mut st = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if GlobalMemoryStatusEx(&mut st).is_ok() {
                return st.ullTotalPhys / (1024 * 1024);
            }
        }
    }
    8192
}

/// Write hardware-qemu.ini overrides are not needed — config.ini suffices.
pub fn wipe_userdata(paths: &Paths) -> Result<()> {
    let dir = paths.avd_dir();
    for name in [
        "userdata-qemu.img",
        "userdata-qemu.img.qcow2",
        "userdata.img",
        "cache.img",
        "cache.img.qcow2",
    ] {
        let p = dir.join(name);
        if p.exists() {
            fs::remove_file(&p).map_err(|e| {
                Error::Emulator(format!("remove {}: {e}", p.display()))
            })?;
        }
    }
    Ok(())
}
