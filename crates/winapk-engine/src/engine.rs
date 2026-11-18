//! Engine: the lifecycle orchestrator replacing upstream engine.js.
//!
//! One shared VM (provisioned once, booted once). Apps run as freeform
//! windows on its display; host windows crop to each app's bounds.
//! All state changes go through the FSM — no implicit-state drift.

use std::collections::HashSet;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use winapk_core::adb::Adb;
use winapk_core::config::{Config, GpuMode};
use winapk_core::error::{Error, Result};
use winapk_core::paths::Paths;
use winapk_core::{debug, err, info, proc, warn};

use crate::accel::{self, Accel};
use crate::avd;
use crate::control::{AdbChannel, AgentCmd, ControlChannel, VirtioSerialChannel};
use crate::fsm::VmState;
use crate::gpu;
use crate::provision::Provisioner;
use crate::tuning;

const BOOT_TIMEOUT: Duration = Duration::from_secs(240);
const QEMU_PASSTHROUGH: &str = "-qemu";

pub struct Engine {
    pub paths: Paths,
    pub config: Config,
    pub state: VmState,
    child: Option<Child>,
    serial: String,
    adb: Option<Adb>,
    channel: Option<Box<dyn ControlChannel>>,
    running_apps: HashSet<String>,
    accel: Accel,
    gpu_mode: GpuMode,
    freeform_ready: bool,
}

impl Engine {
    pub fn new(paths: Paths, config: Config) -> Self {
        Self {
            adb: None,
            channel: None,
            paths,
            config,
            state: VmState::Off,
            child: None,
            serial: String::new(),
            running_apps: HashSet::new(),
            accel: Accel::None,
            gpu_mode: GpuMode::Host,
            freeform_ready: false,
        }
    }

    pub fn adb(&self) -> Option<&Adb> {
        self.adb.as_ref()
    }

    // ---------- provisioning ----------

    /// Download+install runtime pieces if missing. Idempotent.
    pub fn ensure_provisioned<F: Fn(&str, u64, u64)>(&mut self, progress: F) -> Result<()> {
        self.paths.ensure_all()?;
        let prov = Provisioner::new(&self.paths);
        if prov.is_complete() {
            if self.state == VmState::Off {
                self.state.go(VmState::Provisioned)?;
            }
            return Ok(());
        }
        self.state.go(VmState::Provisioning)?;
        let r = prov.provision(progress);
        match r {
            Ok(_) => self.state.go(VmState::Provisioned),
            Err(e) => {
                let _ = self.state.go(VmState::Off);
                Err(e)
            }
        }
    }

    fn ensure_avd(&mut self) -> Result<()> {
        let prov = Provisioner::new(&self.paths);
        let sysimg = prov
            .load_manifest_dir()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| self.paths.sysimg_dir());
        let cfg = self.paths.avd_dir().join("config.ini");
        // Recreate when missing or when the profile marker drifted.
        let stamp = self.paths.avd_dir().join(".profile-stamp");
        let want = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.config.engine.cores,
            self.config.engine.ram_mb,
            self.config.engine.userdata_gb,
            self.config.engine.gpu_mode.as_arg(),
            self.config.engine.lcd_width,
            self.config.engine.lcd_height,
            self.config.engine.lcd_dpi
        );
        let cur = std::fs::read_to_string(&stamp).unwrap_or_default();
        if !cfg.exists() || cur != want {
            info!("(re)writing AVD for profile {want}");
            avd::create(&self.paths, &self.config.engine, &sysimg)?;
            std::fs::write(&stamp, want)?;
        }
        self.ensure_data_partition()?;
        Ok(())
    }

    /// /data must be a REAL ext4 before first boot — the sysimg's
    /// userdata.img is a 1MB zero placeholder and the emulator ships no
    /// mke2fs. Skip this and the guest recovery-loops forever.
    /// Port of upstream ensureDataPartition: seed ext4 → qemu-img grow
    /// (sparse) → bin64/resize2fs → re-set the FBE encrypt flag.
    ///
    /// The FILE is grown to the full ceiling but the FS is only
    /// host-resized to HOST_GROW_CAP — the bundled cygwin resize2fs is
    /// glacial past ~8 GiB. Android's fs_mgr expands the fs to the
    /// partition on first boot (resize flag in fstab).
    fn ensure_data_partition(&mut self) -> Result<()> {
        use crate::userdata;
        const HOST_GROW_CAP: u64 = 8 * 1024 * 1024 * 1024;
        let img = self.paths.avd_dir().join("userdata-qemu.img");
        let want_bytes: u64 = self.config.engine.userdata_gb.max(1) as u64 * (1024 * 1024 * 1024);
        let fs_target = want_bytes.min(HOST_GROW_CAP).max(64 * 1024 * 1024);
        let size = std::fs::metadata(&img).map(|m| m.len()).unwrap_or(0);

        if size > 0 && userdata::has_ext4_superblock(&img) {
            let fsb = userdata::fs_bytes(&img);
            if size >= want_bytes && fsb >= (fs_target as f64 * 0.95) as u64 {
                return Ok(());
            }
            // Grow in place: clear encrypt (old e2fsprogs can't read it),
            // e2fsck for mid-resize crashes, grow file, grow fs, flag back.
            let _ = userdata::clear_encrypt_flag(&img);
            self.e2fsck(&img);
            if size < want_bytes {
                userdata::mark_sparse(&img);
                self.qemu_img_resize(&img, want_bytes)?;
            }
            self.resize2fs(&img, fs_target)?;
            let _ = userdata::set_encrypt_flag(&img);
            info!(
                "userdata grown: file {size} → {want_bytes}, fs {fsb} → {}",
                userdata::fs_bytes(&img)
            );
            return Ok(());
        }

        // Fresh: write the 64MB seed (no encrypt flag yet), then grow.
        // adb_keys is seeded now: playstore images are user builds where
        // the "Allow USB debugging" dialog would otherwise block forever.
        info!("building userdata seed ext4 ({want_bytes} bytes sparse target)");
        let _ = std::fs::remove_file(&img);
        let key = self.host_adb_pubkey();
        let seeds: Vec<userdata::SeedFile> = key
            .as_deref()
            .map(|k| {
                vec![userdata::SeedFile {
                    path: "/misc/adb/adb_keys",
                    data: k,
                    mode: 0o640,
                    uid: 1000, // system
                    gid: 2000, // shell
                }]
            })
            .unwrap_or_default();
        std::fs::write(&img, userdata::build_seed_ext4(64, false, &seeds))?;
        userdata::mark_sparse(&img);
        self.qemu_img_resize(&img, want_bytes)?;
        self.resize2fs(&img, fs_target)?;
        userdata::set_encrypt_flag(&img)?;
        // The seed's inodes carry no SELinux xattrs — files created
        // during first boot can pick a wrong (stale-resolution) label,
        // e.g. /data/system/recoverablekeystore.db-shm as
        // property_data_file → LockSettingsService dies → NMS NPE →
        // system_server crash-loops forever at phase 500. Marker tells
        // wait_boot to run a one-shot `restorecon -R /data` as soon as
        // adbd answers, long before SystemServer reaches phase 500.
        let _ = std::fs::write(self.paths.avd_dir().join(".needs-restorecon"), "1");
        info!(
            "userdata ready: {} GB file, {} GB fs (sparse, FBE-capable)",
            want_bytes >> 30,
            fs_target >> 30
        );
        Ok(())
    }

    /// emulator ships qemu-img.exe next to emulator.exe. Arg order matters:
    /// `qemu-img resize -f raw <file> <bytes>` — `-f` after the subcommand.
    fn qemu_img_resize(&self, img: &std::path::Path, bytes: u64) -> Result<()> {
        let exe = self.paths.emulator_dir().join("qemu-img.exe");
        let out = proc::run(
            std::process::Command::new(exe)
                .arg("resize")
                .arg("-f")
                .arg("raw")
                .arg(img)
                .arg(bytes.to_string()),
            Some(Duration::from_secs(60)),
        )?;
        if !out.status.success() {
            return Err(Error::Provision(format!(
                "qemu-img resize: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    /// Host adb pubkey bytes — %USERPROFILE%\.android\adbkey.pub, the
    /// same file the emulator bakes into cmdline. Generated via
    /// `adb keygen` if absent so the pair stays consistent.
    fn host_adb_pubkey(&self) -> Option<Vec<u8>> {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()?;
        let dir = PathBuf::from(&home).join(".android");
        let pub_f = dir.join("adbkey.pub");
        if !pub_f.exists() {
            std::fs::create_dir_all(&dir).ok()?;
            let mut c = std::process::Command::new(self.paths.adb_exe());
            c.arg("keygen").arg(dir.join("adbkey"));
            let _ = proc::run(&mut c, Some(Duration::from_secs(15)));
        }
        let mut data = std::fs::read(&pub_f).ok()?;
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        Some(data)
    }

    /// e2fsck -fy before resize attempts — a killed resize leaves the fs
    /// dirty and resize2fs then refuses to touch it. Best-effort.
    fn e2fsck(&self, img: &std::path::Path) {
        let bin = self.paths.emulator_dir().join("bin64");
        let exe = bin.join("e2fsck.exe");
        if !exe.exists() {
            return;
        }
        let _ = proc::run(
            std::process::Command::new(exe)
                .arg("-fy")
                .arg(img)
                .current_dir(&bin),
            Some(Duration::from_secs(300)),
        );
    }

    /// resize2fs.exe is a cygwin build — MUST run with cwd=bin64 for its
    /// DLLs. `target` is the fs size ("8G" style); growing beyond ~8 GiB
    /// with this build takes >10 min, so we cap host-side growth.
    fn resize2fs(&self, img: &std::path::Path, target: u64) -> Result<()> {
        let bin = self.paths.emulator_dir().join("bin64");
        let exe = bin.join("resize2fs.exe");
        if !exe.exists() {
            return Err(Error::Provision("bin64/resize2fs.exe missing".into()));
        }
        let out = proc::run(
            std::process::Command::new(exe)
                .arg(img)
                .arg(format!("{}K", target / 1024))
                .current_dir(&bin),
            Some(Duration::from_secs(600)),
        )?;
        if !out.status.success() {
            return Err(Error::Provision(format!(
                "resize2fs: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    /// Adopt an already-running emulator we spawned earlier (CLI is
    /// per-invocation — the VM may outlive the process that started it).
    /// Identity check: our pid file + adb serial whose boot completed.
    /// Returns true if adopted.
    pub fn adopt_running(&mut self) -> Result<bool> {
        let pid: u32 = match std::fs::read_to_string(self.paths.root().join("emulator.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            Some(p) if proc::pid_alive(p) => p,
            _ => return Ok(false),
        };
        let exe = self.paths.adb_exe();
        if !exe.exists() {
            return Ok(false);
        }
        let probe = Adb::new(exe, self.config.engine.adb_port);
        probe.start_server()?;
        for serial in probe.devices()? {
            if !serial.starts_with("emulator-") {
                continue;
            }
            let c = Adb::new(self.paths.adb_exe(), self.config.engine.adb_port)
                .for_serial(&serial);
            let (ok, boot) = c.try_shell("getprop sys.boot_completed", Duration::from_secs(5));
            if ok && boot.trim() == "1" {
                info!("adopted running emulator {serial} (pid {pid})");
                self.serial = serial;
                self.adb = Some(c);
                self.state = VmState::Ready;
                self.freeform_ready = true;
                // The VM may have been started before our tuning existed —
                // apply it on adopt too.
                self.tune_process(pid);
                self.deploy_agent();
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---------- launch ----------

    fn pick_console_port(&self) -> u16 {
        let base = self.config.engine.console_port & !1;
        for p in (base..base + 32).step_by(2) {
            if TcpListener::bind(("127.0.0.1", p)).is_ok()
                && TcpListener::bind(("127.0.0.1", p + 1)).is_ok()
            {
                return p;
            }
        }
        base
    }

    /// Full launch-arg vector. Everything after `-qemu` is QEMU verbatim —
    /// that's where the virtio-serial agent channel and (optional) 9p
    /// share are attached.
    fn launch_args(&self, accel: Accel, gpu: GpuMode, port: u16) -> Vec<String> {
        let e = &self.config.engine;
        let mut a: Vec<String> = vec![
            format!("@{}", avd::AVD_NAME),
            "-port".into(), port.to_string(),
            "-gpu".into(), gpu.as_arg().into(),
            "-accel".into(), accel.arg().into(),
            "-cores".into(), e.resolved_cores(avd::host_logical_cpus()).to_string(),
            "-memory".into(), e.resolved_ram_mb(avd::host_total_mb()).to_string(),
            "-netdelay".into(), "none".into(),
            "-netspeed".into(), "full".into(),
            "-no-metrics".into(),
        ];
        if !e.snapshot {
            a.push("-no-snapshot".into());
        }
        for (k, v) in &e.props {
            a.push("-prop".into());
            a.push(format!("{k}={v}"));
        }
        for f in &e.features {
            a.push("-feature".into());
            a.push(f.clone());
        }
        // --- QEMU verbatim tail ---
        a.push(QEMU_PASSTHROUGH.into());
        // virtio-serial agent channel (host side = TCP listen).
        a.extend([
            "-chardev".into(),
            format!(
                "socket,server=on,wait=off,host=127.0.0.1,port={},id=winapk",
                e.agent_port
            ),
            "-device".into(),
            "virtio-serial-pci".into(),
            "-device".into(),
            "virtserialport,chardev=winapk,name=winapk.0".into(),
        ]);
        if e.virtio_fs {
            // virtio-9p share (works on stock kernels with 9p modules; the
            // agent mounts tag 'winapkshare' → /sdcard/WINapkShare).
            a.extend([
                "-virtfs".into(),
                format!(
                    "local,path={},mount_tag=winapkshare,security_model=none,id=winapkshare,readonly=off",
                    self.paths.share_dir().to_string_lossy().replace('\\', "/")
                ),
            ]);
        }
        // QMP monitor: guest input injection for the ADB auth dialog on
        // user builds, and later for the display host's input pipeline.
        a.extend([
            "-qmp".into(),
            format!("tcp:127.0.0.1:{},server=on,wait=off", e.agent_port + 1),
        ]);
        a
    }

    fn spawn_emulator(&mut self, accel: Accel, gpu: GpuMode) -> Result<u32> {
        let port = self.pick_console_port();
        let args = self.launch_args(accel, gpu, port);
        let mut c = std::process::Command::new(self.paths.emulator_exe());
        c.args(&args)
            .current_dir(self.paths.emulator_dir())
            .env("ANDROID_SDK_ROOT", self.paths.runtime())
            .env("ANDROID_SDK_HOME", self.paths.root())
            .env("ANDROID_AVD_HOME", self.paths.avd_home())
            .env("ANDROID_EMULATOR_HOME", self.paths.avd_home())
            .env("ADB_SERVER_PORT", self.config.engine.adb_port.to_string())
            .env("ANDROID_ADB_SERVER_PORT", self.config.engine.adb_port.to_string())
            .env("ANDROID_HOME", self.paths.runtime());
        let log = self.paths.logs_dir().join("emulator.log");
        info!("spawn emulator -accel {} -gpu {} -port {}", accel.arg(), gpu.as_arg(), port);
        debug!("args: {}", args.join(" "));
        let child = proc::spawn_logged(&mut c, &log)?;
        let pid = child.id();
        let _ = std::fs::write(self.paths.root().join("emulator.pid"), pid.to_string());
        self.child = Some(child);
        self.serial = format!("emulator-{port}");
        self.spawn_job_keeper(pid);
        self.spawn_auth_guard();
        Ok(pid)
    }

    /// The CLI exits long before the VM does, so the Job Object must be
    /// owned by a process that shares the VM's lifetime: a detached
    /// `winapk __vmd <pid>` keeper wraps the emulator pid in a
    /// KILL_ON_JOB_CLOSE job and blocks until the VM exits. Keeper dies
    /// → job closes → whole QEMU tree dies (no orphans, ever). VM dies
    /// → keeper exits. The future winapkd absorbs this role directly.
    fn spawn_job_keeper(&self, pid: u32) {
        let Ok(exe) = std::env::current_exe() else { return };
        let mut c = std::process::Command::new(exe);
        c.arg("__vmd").arg(pid.to_string());
        match proc::spawn_quiet(&mut c) {
            Ok(_) => info!("job-keeper __vmd watching pid {pid}"),
            Err(e) => warn!("job keeper spawn failed (orphan sweep remains): {e}"),
        }
    }

    /// Background guard: while this VM lives, if adb reports the device
    /// as "unauthorized" (user-build ro.adb.secure=1 dialog), drive the
    /// QEMU input pipeline to accept it. Seeded adb_keys usually makes
    /// this a no-op; the tap is the deterministic fallback.
    fn spawn_auth_guard(&self) {
        let exe = self.paths.adb_exe();
        let port = self.config.engine.adb_port;
        let serial = self.serial.clone();
        let qmp_port = self.config.engine.agent_port + 1;
        let (w, h) = (self.config.engine.lcd_width, self.config.engine.lcd_height);
        let pid = self.child.as_ref().map(|c| c.id()).unwrap_or(0);
        std::thread::spawn(move || {
            let adb = Adb::new(exe, port);
            let _ = adb.run(&["start-server"], Duration::from_secs(15));
            let mut qmp: Option<crate::qmp::Qmp> = None;
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            let mut taps = 0u32;
            while std::time::Instant::now() < deadline && proc::pid_alive(pid) {
                match adb.device_state(&serial).as_deref() {
                    Some("unauthorized") if taps < 12 => {
                        if qmp.is_none() {
                            qmp = crate::qmp::Qmp::connect(
                                qmp_port,
                                Duration::from_secs(4),
                            )
                            .ok();
                        }
                        if let Some(q) = qmp.as_mut() {
                            // Dialog layout (AOSP USB-debug confirm), as
                            // fractions of the framebuffer:
                            //   "Always allow" checkbox ~(0.13, 0.575)
                            //   "Allow" button         ~(0.88, 0.671)
                            let _ = q.tap(
                                (w as f32 * 0.13) as u32,
                                (h as f32 * 0.575) as u32,
                                w,
                                h,
                            );
                            let _ = q.tap(
                                (w as f32 * 0.88) as u32,
                                (h as f32 * 0.671) as u32,
                                w,
                                h,
                            );
                            taps += 1;
                            info!("adb auth dialog: clicked Allow via QMP (tap {taps})");
                            std::thread::sleep(Duration::from_secs(3));
                            continue;
                        }
                    }
                    Some("device") => return, // authorized — done
                    _ => {}
                }
                std::thread::sleep(Duration::from_secs(4));
            }
        });
    }

    /// pid of the emulator process we spawned (for the display host).
    pub fn emulator_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// Apply the Win32 tuning layer to a live emulator pid.
    fn tune_process(&self, pid: u32) {
        if self.config.engine.no_ecoqos && tuning::disarm_power_throttling(pid) {
            info!("EcoQoS disabled on pid {pid}");
        }
        if self.config.engine.pin_p_cores && tuning::pin_to_fastest_cores(pid) {
            info!("vCPU threads pinned to P-core cpu sets");
        }
        tuning::set_priority(pid, true);
        tuning::prevent_sleep(true);
    }

    /// Boot the VM if needed and wait for Android ready.
    /// GPU fallback chain: preferred mode, then next in chain on boot
    /// failure (child died early) — swiftshader is last-resort only.
    pub fn ensure_running(&mut self) -> Result<()> {
        // Cross-process serialization: a second winapk.exe blocks here
        // instead of racing our provision/adopt/boot writes. The guard
        // is held for the whole call — the loser re-runs ensure_running
        // afterwards and simply adopts the now-live VM.
        let _vm_lock = proc::acquire_named_mutex("Global\\WINapk-vm");
        self.ensure_provisioned(|_, _, _| {})?;
        // Reattach to a VM an earlier CLI invocation already booted —
        // BEFORE ensure_avd: adoption must not be blocked by userdata
        // provisioning work (e.g. a slow e2fsck) the running VM doesn't need.
        if self.state == VmState::Provisioned && self.adopt_running()? {
            self.channel = Some(self.open_channel());
            return Ok(());
        }
        self.ensure_avd()?;
        match self.state {
            VmState::Ready | VmState::AppRunning => return Ok(()),
            VmState::Booting => return self.wait_boot(BOOT_TIMEOUT),
            VmState::Provisioned => {}
            s => {
                return Err(Error::Emulator(format!("can't boot from state {s}")));
            }
        }

        let (accel, check_log) = accel::accel_check(&self.paths.emulator_exe());
        if accel == Accel::None {
            return Err(Error::Emulator(format!(
                "no hardware acceleration (install WHPX feature or AEHD):\n{check_log}"
            )));
        }
        self.accel = accel;
        info!("accelerator: {:?} (gvm_installed={})", accel, accel::gvm_installed());

        let gpus = gpu::detect();
        let preferred = if self.config.engine.gpu_mode == GpuMode::Swiftshader {
            GpuMode::Swiftshader
        } else if self.config.engine.gpu_mode == GpuMode::Host {
            gpu::preferred_mode(&gpus)
        } else {
            self.config.engine.gpu_mode
        };
        let mut chain: Vec<GpuMode> = vec![preferred];
        for m in GpuMode::fallback_chain() {
            if !chain.contains(m) && *m != GpuMode::Swiftshader {
                chain.push(*m);
            }
        }
        chain.push(GpuMode::Swiftshader);

        let mut last_err = Error::Emulator("unknown".into());
        for (i, mode) in chain.iter().enumerate() {
            self.state.go(VmState::Launching)?;
            let pid = match self.spawn_emulator(self.accel, *mode) {
                Ok(p) => p,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            self.tune_process(pid);
            self.gpu_mode = *mode;
            self.state.go(VmState::Booting)?;
            // Fail fast on GPU modes we only *suspect* work — a driver
            // crash/dialog hangs the emulator, and waiting 4 min per
            // attempt makes the fallback chain useless. The final
            // (swiftshader) attempt gets the full timeout: it always
            // boots, just slowly.
            let last = i == chain.len() - 1;
            let timeout = if last {
                BOOT_TIMEOUT
            } else {
                Duration::from_secs(90)
            };
            match self.wait_boot(timeout) {
                Ok(()) => {
                    if i > 0 {
                        warn!("GPU mode fell back to {}", mode.as_arg());
                    }
                    return Ok(());
                }
                Err(e) => {
                    err!("boot failed on -gpu {}: {e}", mode.as_arg());
                    last_err = e;
                    self.kill_child();
                    // wait_boot leaves us Booting or Failed; both need
                    // Failed → Off → Provisioned to re-allow Launching.
                    let _ = self.state.go(VmState::Failed);
                    let _ = self.state.go(VmState::Off);
                    let _ = self.state.go(VmState::Provisioned);
                }
            }
        }
        let _ = self.state.go(VmState::Failed);
        Err(last_err)
    }

    fn wait_boot(&mut self, timeout: Duration) -> Result<()> {
        let adb = self.adb_client()?;
        adb.start_server()?;
        // Fail fast if the emulator process already exited (driver crash).
        let start = std::time::Instant::now();
        // `granted` grows whenever the boot proves it's alive — either
        // the device row exists at all (a guest that is adbd-connected,
        // even "unauthorized" while the auth guard clicks the dialog)
        // or emulator.log is still growing (QEMU writing boot progress
        // before adbd registers — slow hosts can take >90s to first
        // contact). A hung GPU init stops BOTH signals and dies on the
        // short timeout; a slow-but-progressing boot keeps extending up
        // to ABS_CAP — this is what makes a ~5min cold boot on a weak
        // host survive the "fail fast" speculative GPU timeout.
        const ABS_CAP: Duration = Duration::from_secs(1200);
        let mut granted = timeout;
        let hard_cap = (timeout * 3).max(BOOT_TIMEOUT).max(ABS_CAP);
        let log = self.paths.logs_dir().join("emulator.log");
        let mut last_len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        loop {
            if let Some(ch) = self.child.as_mut() {
                if let Ok(Some(st)) = ch.try_wait() {
                    let _ = self.state.go(VmState::Failed);
                    return Err(Error::Emulator(format!(
                        "emulator exited during boot: {st} — see logs/emulator.log"
                    )));
                }
            }
            let mut alive = false;
            match adb.device_state(&self.serial).as_deref() {
                Some("device") => {
                    alive = true;
                    self.maybe_restorecon(&adb);
                }
                Some("unauthorized") | Some("offline") => alive = true,
                _ => {}
            }
            let len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
            if len > last_len {
                last_len = len;
                alive = true;
            }
            if alive {
                let want = start.elapsed() + Duration::from_secs(120);
                granted = granted.max(want).min(hard_cap);
            }
            if start.elapsed() > granted {
                return Err(Error::Timeout("adb wait-for-device".into()));
            }
            // Slice wait_ready: it blocks internally until ITS deadline,
            // so handing it the whole grant in one call would starve the
            // liveness checks above — re-evaluate every ≤20s instead.
            let slice = granted
                .saturating_sub(start.elapsed())
                .min(Duration::from_secs(20));
            match adb.wait_ready(slice) {
                Ok(()) => {
                    self.state.go(VmState::Ready)?;
                    info!("Android ready on {}", self.serial);
                    let adb2 = self.adb_client()?;
                    adb2.enable_freeform();
                    self.deploy_agent();
                    self.channel = Some(self.open_channel());
                    // vCPU/render threads only exist AFTER boot —
                    // pinning at CreateProcess hit nothing. Re-pin now
                    // against the real qemu-system process tree.
                    if let Some(pid) = self.emulator_pid() {
                        tuning::repin_guest_threads(pid);
                    }
                    return Ok(());
                }
                Err(Error::Timeout(_)) => {
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One-shot first-boot fixup: relabel /data as soon as adbd answers.
    /// Detached — restorecon walks the whole fs (~30-60s in-guest) and
    /// must race SystemServer to LockSettingsService's db creation,
    /// so it fires on the FIRST "device" sighting, not after boot.
    /// `su 0` only exists on userdebug; on user builds it's a no-op.
    fn maybe_restorecon(&self, _adb: &Adb) {
        let marker = self.paths.avd_dir().join(".needs-restorecon");
        if !marker.exists() {
            return;
        }
        if std::fs::remove_file(&marker).is_err() {
            return;
        }
        let exe = self.paths.adb_exe();
        let port = self.config.engine.adb_port;
        let serial = self.serial.clone();
        std::thread::spawn(move || {
            let a = Adb::new(exe, port).for_serial(&serial);
            let (ok, out) = a.try_shell(
                "su 0 restorecon -R /data",
                Duration::from_secs(180),
            );
            info!("first-boot restorecon: ok={ok} {}", out.trim());
            // The Google integrity verifier times out on a cold first
            // boot (INSTALL_FAILED_VERIFICATION_FAILURE) — this is a
            // headless dev image, verification buys nothing.
            let _ = a.try_shell(
                "su 0 settings put global package_verifier_enable 0; \
                 su 0 settings put global verifier_verify_adb_installs 0",
                Duration::from_secs(30),
            );
        });
    }

    /// Push the musl-static agent to /data/local/tmp (idempotent — size
    /// check skips re-push), relaunch it detached. Runs as `su 0` on
    /// userdebug images (shell context on user builds). The agent owns
    /// the virtio-serial port; once it's up, open_channel()'s ping
    /// upgrades us off the adb fallback automatically.
    fn deploy_agent(&self) {
        const DEST: &str = "/data/local/tmp/winapk-agent";
        let Some(local) = self.paths.agent_bin() else {
            info!("no winapk-agent binary found — adb channel only");
            return;
        };
        let Ok(adb) = self.adb_client() else { return };
        let local_len = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
        let (_, size) = adb.try_shell(
            &format!("stat -c %s {DEST} 2>/dev/null || echo 0"),
            Duration::from_secs(5),
        );
        let remote_len: u64 = size.trim().parse().unwrap_or(0);
        if remote_len != local_len {
            info!("deploying agent to {DEST} ({local_len} bytes)");
            if adb.push(&local, DEST, Duration::from_secs(120)).is_err() {
                warn!("agent push failed");
                return;
            }
            let _ = adb.try_shell(&format!("chmod 755 {DEST}"), Duration::from_secs(5));
        }
        // Restart idempotently; root when the image gives it to us.
        // NOTE: kill by pidof, never `pkill -f` — the -f pattern matches
        // the adb shell's own cmdline and SIGTERMs our own session.
        let (ok, uid) = adb.try_shell("su 0 id -u", Duration::from_secs(5));
        let root = ok && uid.trim() == "0";
        let kill = if root {
            "su 0 kill $(pidof winapk-agent) 2>/dev/null"
        } else {
            "kill $(pidof winapk-agent) 2>/dev/null"
        };
        let _ = adb.try_shell(kill, Duration::from_secs(5));
        let cmd = if root {
            format!("su 0 {DEST}")
        } else {
            DEST.to_string()
        };
        adb.spawn_detached(&cmd);
    }

    fn adb_client(&self) -> Result<Adb> {
        let exe = self.paths.adb_exe();
        if !exe.exists() {
            return Err(Error::Provision("adb.exe not provisioned".into()));
        }
        Ok(Adb::new(exe, self.config.engine.adb_port).for_serial(&self.serial))
    }

    /// Open the best available control channel: virtio-serial if the guest
    /// agent answers a ping, else the ADB fallback channel.
    fn open_channel(&self) -> Box<dyn ControlChannel> {
        let mut vs = VirtioSerialChannel::new(self.config.engine.agent_port);
        if vs
            .call(&AgentCmd::Ping, Duration::from_secs(2))
            .is_ok()
        {
            info!("control channel: virtio-serial agent");
            return Box::new(vs);
        }
        info!("control channel: adb fallback");
        Box::new(AdbChannel::new(self.adb_client().unwrap()))
    }

    // ---------- app sessions ----------

    /// Launch an installed package: ensure VM → freeform → am start →
    /// returns (serial, freeform_bounds) so the display layer can attach.
    pub fn run_app(&mut self, pkg: &str) -> Result<()> {
        self.ensure_running()?;
        let adb = self.adb_client()?;
        if !adb.is_installed(pkg) {
            return Err(Error::Adb(format!("package not installed: {pkg}")));
        }
        if !self.freeform_ready {
            adb.enable_freeform();
            self.freeform_ready = true;
        }
        if let Some(ch) = self.channel.as_mut() {
            ch.call(
                &AgentCmd::Start {
                    pkg: pkg.to_string(),
                    activity: None,
                    freeform: true,
                },
                Duration::from_secs(30),
            )?;
        }
        self.unidle();
        self.running_apps.insert(pkg.to_string());
        if self.state == VmState::Ready {
            self.state.go(VmState::AppRunning)?;
        }
        Ok(())
    }

    /// Called when an app's host window closes.
    pub fn app_closed(&mut self, pkg: &str) -> Result<()> {
        self.running_apps.remove(pkg);
        if self.running_apps.is_empty() && self.state == VmState::AppRunning {
            self.state.go(VmState::Ready)?;
            // Idle: give the CPU back to the desktop — this replaces
            // upstream's "dump GiB of RAM to ram.bin" approach entirely.
            if let Some(ch) = self.child.as_ref() {
                tuning::set_priority(ch.id(), false);
            }
            info!("no apps foreground → VM idling (priority lowered)");
        }
        Ok(())
    }

    /// Wake from idle when a new app attaches.
    fn unidle(&mut self) {
        if let Some(ch) = self.child.as_ref() {
            tuning::set_priority(ch.id(), true);
        }
    }

    /// Graceful stop: ask agent/emulator to power off, escalate to taskkill.
    pub fn stop(&mut self) -> Result<()> {
        // Same named mutex as ensure_running — stop can't interleave
        // with another process's adopt/boot.
        let _vm_lock = proc::acquire_named_mutex("Global\\WINapk-vm");
        // Adopt a VM started by a previous CLI run, so `winapk stop` works.
        if self.child.is_none() {
            let _ = self.adopt_running();
        }
        if self.child.is_none() {
            // Adopted via pid file: kill adb-side then the pid itself.
            let pid: Option<u32> = std::fs::read_to_string(
                self.paths.root().join("emulator.pid"),
            )
            .ok()
            .and_then(|s| s.trim().parse().ok());
            if let Some(adb) = self.adb.as_ref() {
                let _ = adb.try_run(&["emu", "kill"], Duration::from_secs(10));
            }
            if let Some(p) = pid {
                std::thread::sleep(Duration::from_secs(6));
                if proc::pid_alive(p) {
                    proc::kill_tree(p);
                }
            }
            let _ = std::fs::remove_file(self.paths.root().join("emulator.pid"));
            self.state = VmState::Off;
            tuning::prevent_sleep(false);
            return Ok(());
        }
        let _ = self.state.go(VmState::Stopping);
        if let Some(adb) = self.adb.as_ref() {
            let _ = adb.try_run(&["emu", "kill"], Duration::from_secs(10));
        }
        self.kill_child();
        tuning::prevent_sleep(false);
        self.channel = None;
        self.running_apps.clear();
        let _ = self.state.go(VmState::Off);
        Ok(())
    }

    fn kill_child(&mut self) {
        if let Some(mut ch) = self.child.take() {
            let pid = ch.id();
            if ch.try_wait().ok().flatten().is_none() {
                // give graceful kill 8s, then taskkill /T
                std::thread::sleep(Duration::from_secs(8));
                if ch.try_wait().ok().flatten().is_none() {
                    proc::kill_tree(pid);
                }
            }
            let _ = ch.wait();
        }
        // emulator.exe re-execs detached — the real QEMU escapes the
        // tree kill and would hold the console port + burn a core into
        // the next GPU attempt. Sweep by AVD tag.
        proc::kill_emulator_orphans(&format!("@{}", avd::AVD_NAME));
        let _ = std::fs::remove_file(self.paths.root().join("emulator.pid"));
    }

    /// Refresh the cached adb handle (serial can change if port moved).
    pub fn bind_adb(&mut self) -> Result<()> {
        self.adb = Some(self.adb_client()?);
        Ok(())
    }

    // ---------- diagnostics ----------

    pub fn doctor(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Report REAL liveness, not this process's FSM — the CLI is
        // per-invocation so self.state is always Off even when a VM we
        // spawned earlier is still up.
        let live = std::fs::read_to_string(self.paths.root().join("emulator.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| proc::pid_alive(*p));
        let state = match live {
            None => "off".to_string(),
            Some(pid) => {
                let adb_state = if self.paths.adb_exe().exists() {
                    let probe =
                        Adb::new(self.paths.adb_exe(), self.config.engine.adb_port);
                    let _ = probe.start_server();
                    probe
                        .serials()
                        .into_iter()
                        .find(|s| s.starts_with("emulator-"))
                        .and_then(|s| probe.device_state(&s))
                        .unwrap_or_else(|| "?".into())
                } else {
                    "?".into()
                };
                format!("running (pid {pid}, adb {adb_state})")
            }
        };
        out.push(format!("state: {state}"));
        out.push(format!("home: {}", self.paths.root().display()));
        let s = Provisioner::new(&self.paths).status();
        out.push(format!(
            "runtime: platform-tools={} emulator={} sysimg={}",
            s.platform_tools, s.emulator, s.sysimg
        ));
        out.push(format!("cpus: {} logical", avd::host_logical_cpus()));
        out.push(format!("ram: {} MB", avd::host_total_mb()));
        let (acc, log) = accel::accel_check(&self.paths.emulator_exe());
        out.push(format!("accel: {:?} (gvm={})", acc, accel::gvm_installed()));
        for l in log.lines().filter(|l| !l.trim().is_empty()).take(6) {
            out.push(format!("  {l}"));
        }
        for (v, d) in gpu::detect() {
            out.push(format!("gpu: {:?} — {d}", v));
        }
        let ids = tuning::fastest_cpu_set_ids();
        out.push(format!(
            "hybrid-pcores: {} ({} sets)",
            !ids.is_empty(),
            ids.len()
        ));
        out
    }
}
