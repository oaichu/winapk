use std::path::{Path, PathBuf};

/// On-disk layout under %LOCALAPPDATA%\WINapk. Overridable via WINAPK_HOME
/// so tests and portable installs don't touch the real profile.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new() -> std::io::Result<Self> {
        let root = std::env::var("WINAPK_HOME").map_or_else(
            |_| {
                let la = std::env::var("LOCALAPPDATA")
                    .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into());
                Path::new(&la).join("WINapk")
            },
            PathBuf::from,
        );
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn store_file(&self) -> PathBuf {
        self.root.join("store.json")
    }
    pub fn runtime(&self) -> PathBuf {
        self.root.join("runtime")
    }
    pub fn runtime_manifest(&self) -> PathBuf {
        self.runtime().join("manifest.json")
    }
    pub fn emulator_dir(&self) -> PathBuf {
        self.runtime().join("emulator")
    }
    pub fn emulator_exe(&self) -> PathBuf {
        self.emulator_dir().join("emulator.exe")
    }
    pub fn platform_tools(&self) -> PathBuf {
        self.runtime().join("platform-tools")
    }
    pub fn adb_exe(&self) -> PathBuf {
        self.platform_tools().join("adb.exe")
    }
    pub fn system_images(&self) -> PathBuf {
        self.runtime().join("system-images")
    }
    /// api-34 google_apis (userdebug) x86_64 image dir (after extraction).
    /// userdebug — not playstore `user` — so adbd trusts seeded keys and
    /// never shows the interactive RSA-auth dialog.
    pub fn sysimg_dir(&self) -> PathBuf {
        self.system_images()
            .join("android-34")
            .join("google_apis")
            .join("x86_64")
    }
    pub fn aehd_dir(&self) -> PathBuf {
        self.runtime().join("aehd")
    }
    pub fn avd_home(&self) -> PathBuf {
        self.root.join("avd")
    }
    pub fn avd_dir(&self) -> PathBuf {
        self.avd_home().join("WINapk.avd")
    }
    pub fn avd_ini(&self) -> PathBuf {
        self.avd_home().join("WINapk.ini")
    }
    pub fn share_dir(&self) -> PathBuf {
        self.root.join("share")
    }
    pub fn share_manifest(&self) -> PathBuf {
        self.share_dir().join(".winapk-sync.json")
    }
    pub fn icons_dir(&self) -> PathBuf {
        self.root.join("icons")
    }
    pub fn icon_ico(&self, pkg: &str) -> PathBuf {
        self.icons_dir().join(format!("{pkg}.ico"))
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub fn display_host_exe(&self) -> PathBuf {
        // Look next to winapk.exe first (installed layout), then workspace.
        if let Ok(exe) = std::env::current_exe() {
            let cand = exe.with_file_name("winapk-display-host.exe");
            if cand.exists() {
                return cand;
            }
        }
        self.root.join("bin").join("winapk-display-host.exe")
    }

    pub fn ensure_all(&self) -> std::io::Result<()> {
        for d in [
            self.runtime(),
            self.avd_home(),
            self.share_dir(),
            self.icons_dir(),
            self.logs_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }
}
