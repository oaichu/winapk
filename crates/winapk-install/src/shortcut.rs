//! Desktop .lnk via IShellLinkW COM + .apk file association via registry.

use std::path::Path;

use windows::core::{HSTRING, Interface};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SHGetKnownFolderPath, ShellLink, FOLDERID_Desktop, KF_FLAG_DEFAULT,
};

use winapk_core::error::{Error, Result};

fn desktop_dir() -> Result<std::path::PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)
            .map_err(|e| Error::Apk(format!("desktop dir: {e}")))?;
        let s = p.to_string().map_err(|e| Error::Apk(format!("{e}")))?;
        windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr() as *const _));
        Ok(s.into())
    }
}

/// Create/overwrite `<Desktop>\<label>.lnk` → `winapk run <pkg>`.
pub fn create_shortcut(label: &str, pkg: &str, winapk_exe: &Path, ico: &Path) -> Result<std::path::PathBuf> {
    let desktop = desktop_dir()?;
    let safe: String = label
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    let lnk = desktop.join(format!("{}.lnk", if safe.is_empty() { pkg } else { &safe }));

    unsafe {
        let _init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| Error::Apk(format!("IShellLink: {e}")))?;
        link.SetPath(&HSTRING::from(winapk_exe.to_string_lossy().as_ref()))
            .map_err(|e| Error::Apk(format!("SetPath: {e}")))?;
        link.SetArguments(&HSTRING::from(format!("run {pkg}")))
            .map_err(|e| Error::Apk(format!("SetArguments: {e}")))?;
        link.SetDescription(&HSTRING::from(format!("{label} — Android via WINapk")))
            .ok();
        link.SetIconLocation(&HSTRING::from(ico.to_string_lossy().as_ref()), 0)
            .ok();
        link.SetWorkingDirectory(&HSTRING::from(
            winapk_exe
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ))
        .ok();
        let pf: IPersistFile = link.cast().map_err(|e| Error::Apk(format!("IPersistFile: {e}")))?;
        pf.Save(&HSTRING::from(lnk.to_string_lossy().as_ref()), true)
            .map_err(|e| Error::Apk(format!("Save lnk: {e}")))?;
        CoUninitialize();
    }
    Ok(lnk)
}

fn classes_key(sub: &str) -> String {
    format!("Software\\Classes\\{sub}")
}

/// Register `.apk` → "Install with WINapk" under HKCU (no admin needed).
pub fn assoc_on(winapk_exe: &Path) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe = winapk_exe.to_string_lossy().replace('/', "\\");

    let (k, _) = hkcu
        .create_subkey(&classes_key(".apk"))
        .map_err(|e| Error::Apk(format!("reg .apk: {e}")))?;
    k.set_value("", &"WINapk.ApkFile")
        .map_err(|e| Error::Apk(format!("reg .apk set: {e}")))?;

    let (k, _) = hkcu
        .create_subkey(&classes_key("WINapk.ApkFile"))
        .map_err(|e| Error::Apk(format!("reg progid: {e}")))?;
    k.set_value("", &"Android Package (WINapk)")
        .map_err(|e| Error::Apk(format!("reg progid set: {e}")))?;

    let (k, _) = hkcu
        .create_subkey(&classes_key("WINapk.ApkFile\\DefaultIcon"))
        .map_err(|e| Error::Apk(format!("reg icon: {e}")))?;
    k.set_value("", &format!("{exe},0"))
        .map_err(|e| Error::Apk(format!("reg icon set: {e}")))?;

    let (k, _) = hkcu
        .create_subkey(&classes_key("WINapk.ApkFile\\shell\\open\\command"))
        .map_err(|e| Error::Apk(format!("reg cmd: {e}")))?;
    k.set_value("", &format!("\"{exe}\" install \"%1\""))
        .map_err(|e| Error::Apk(format!("reg cmd set: {e}")))?;
    Ok(())
}

pub fn assoc_off() -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(&classes_key("WINapk.ApkFile"));
    if let Ok(k) = hkcu.open_subkey_with_flags(
        &classes_key(".apk"),
        winreg::enums::KEY_SET_VALUE,
    ) {
        let cur: String = k.get_value("").unwrap_or_default();
        if cur == "WINapk.ApkFile" {
            let _ = k.delete_value("");
        }
    }
    Ok(())
}
