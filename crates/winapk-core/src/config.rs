use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    /// OpenGL ES / Vulkan passthrough to the physical GPU (gfxstream).
    Host,
    /// ANGLE→D3D11 indirect; fallback for broken vendor GL drivers.
    AngleIndirect,
    /// Pure-CPU rasterizer. Last resort only.
    Swiftshader,
}

impl GpuMode {
    pub fn as_arg(self) -> &'static str {
        match self {
            GpuMode::Host => "host",
            GpuMode::AngleIndirect => "angle_indirect",
            GpuMode::Swiftshader => "swiftshader_indirect",
        }
    }

    /// Probe order used when the user hasn't pinned a mode.
    pub fn fallback_chain() -> &'static [GpuMode] {
        &[
            GpuMode::Host,
            GpuMode::AngleIndirect,
            GpuMode::Swiftshader,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineProfile {
    /// vCPU count. 0 = auto (half of physical, clamped 4..12).
    pub cores: u32,
    /// Guest RAM MiB. 0 = auto (~35% of host, clamped 3072..12288).
    pub ram_mb: u32,
    /// Sparse userdata partition ceiling, GiB (qcow2 — real usage grows on demand).
    pub userdata_gb: u32,
    /// Preferred GPU backend; chain order is used on boot failure.
    pub gpu_mode: GpuMode,
    /// Keep quick-boot snapshots. Default OFF — snapshots write GiB-scale
    /// ram.bin on every exit and rot silently on hard shutdown.
    pub snapshot: bool,
    /// LCD geometry for the primary display.
    pub lcd_width: u32,
    pub lcd_height: u32,
    pub lcd_dpi: u32,
    /// System properties injected via `-prop` at launch. Defaults include
    /// the ART x86-64-v3 ISA hints (see default_props()).
    pub props: Vec<(String, String)>,
    /// Emulator `-feature` flags (e.g. "Vulkan", "-GLDirectMem").
    pub features: Vec<String>,
    /// virtio-serial control port for the guest agent.
    pub agent_port: u16,
    /// adb server port — deliberately NOT 5037 to coexist with the user's SDK.
    pub adb_port: u16,
    /// Console port range start (even, 5554..5584).
    pub console_port: u16,
    /// Pin vCPU threads to the highest-EfficiencyClass core cluster (P-cores).
    pub pin_p_cores: bool,
    /// Disable Windows power throttling (EcoQoS) on the emulator process.
    pub no_ecoqos: bool,
    /// Mount the shared folder over virtio-9p (`-virtfs`) — requires a
    /// kernel/image that supports it; adb-sync remains the default.
    pub virtio_fs: bool,
}

impl Default for EngineProfile {
    fn default() -> Self {
        Self {
            cores: 0,
            ram_mb: 0,
            userdata_gb: 64,
            gpu_mode: GpuMode::Host,
            snapshot: false,
            lcd_width: 1080,
            lcd_height: 1920,
            lcd_dpi: 420,
            props: Self::default_props(),
            features: vec![],
            agent_port: 45678,
            adb_port: 5038,
            console_port: 5554,
            pin_p_cores: true,
            no_ecoqos: true,
            virtio_fs: false,
        }
    }
}

impl EngineProfile {
    /// ART ISA hints: let dex2oat emit x86-64-v3 (AVX2/FMA/BMI2) code.
    /// Harmless on images that ignore them; gated by config so a custom
    /// image can tighten or extend the set.
    ///
    /// Emulator ≥35 only forwards `qemu.*` props via -prop, so these must
    /// carry that prefix — the guest sees them as `qemu.dalvik…`, and
    /// ART reads the unprefixed name only on images that mirror them
    /// (custom AOSP phase; harmless noise on stock images).
    pub fn default_props() -> Vec<(String, String)> {
        vec![
            (
                "qemu.dalvik.vm.isa.x86_64.variant".into(),
                "x86_64".into(),
            ),
            (
                "qemu.dalvik.vm.isa.x86_64.features".into(),
                "avx,avx2,fma,bmi1,bmi2,popcnt,lzcnt,movbe,aes,sse4_1,sse4_2".into(),
            ),
            // Prefer native .so over the translation layer when both exist.
            ("qemu.ro.dalvik.vm.native.bridge".into(), "default".into()),
        ]
    }

    pub fn resolved_cores(&self, host_logical: u32) -> u32 {
        if self.cores > 0 {
            self.cores
        } else {
            (host_logical / 2).clamp(4, 12)
        }
    }

    pub fn resolved_ram_mb(&self, host_total_mb: u64) -> u32 {
        if self.ram_mb > 0 {
            self.ram_mb
        } else {
            let mb = ((host_total_mb as f64 * 0.35) / 256.0) as u64 * 256;
            mb.clamp(3072, 12288) as u32
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: EngineProfile,
    /// Freeform window bounds (portrait) used for per-app cropping.
    pub window: WindowConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// "per-app" (host window per app, cropped) or "desktop" (whole screen).
    pub mode: String,
    pub portrait_w: u32,
    pub portrait_h: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            mode: "per-app".into(),
            portrait_w: 540,
            portrait_h: 960,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
        toml::from_str(&text)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }
}
