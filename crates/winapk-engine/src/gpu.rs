//! Host GPU vendor detection via the display-class registry key.
//! Used to pick the initial `-gpu` mode: known-bad Intel GL drivers
//! start on angle_indirect instead of discovering the crash on boot.

use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Other,
    Unknown,
}

const DISPLAY_CLASS: &str =
    "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";

pub fn detect() -> Vec<(GpuVendor, String)> {
    let mut gpus = Vec::new();
    let Ok(class) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(DISPLAY_CLASS) else {
        return gpus;
    };
    for name in class.enum_keys().flatten() {
        let Ok(k) = class.open_subkey(&name) else {
            continue;
        };
        let id: String = k.get_value("MatchingDeviceId").unwrap_or_default();
        if !id.contains("pci\\ven_") {
            continue; // skip non-GPU class members
        }
        let desc: String = k
            .get_value("DriverDesc")
            .or_else(|_| k.get_value("HardwareInformation.AdapterString"))
            .unwrap_or_else(|_| "unknown".into());
        let vendor = if id.contains("ven_10de") {
            GpuVendor::Nvidia
        } else if id.contains("ven_1002") || id.contains("ven_1022") {
            GpuVendor::Amd
        } else if id.contains("ven_8086") {
            GpuVendor::Intel
        } else {
            GpuVendor::Other
        };
        gpus.push((vendor, desc));
    }
    gpus
}

/// Prefer angle_indirect on old Intel iGPUs (pre-Xe GL stacks crash
/// gfxstream), host otherwise.
pub fn preferred_mode(
    gpus: &[(GpuVendor, String)],
) -> winapk_core::config::GpuMode {
    use winapk_core::config::GpuMode;
    if gpus.iter().any(|(v, _)| *v == GpuVendor::Nvidia || *v == GpuVendor::Amd) {
        return GpuMode::Host;
    }
    if let Some((_, desc)) = gpus.iter().find(|(v, _)| *v == GpuVendor::Intel) {
        // Xe/Arc-era drivers are fine on host GL; older UHD go via ANGLE.
        let d = desc.to_lowercase();
        if d.contains("arc") || d.contains("iris xe") {
            return GpuMode::Host;
        }
        return GpuMode::AngleIndirect;
    }
    GpuMode::Host
}
