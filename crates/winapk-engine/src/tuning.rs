//! Win32 host tuning for the emulator process — the layer apkstart never had:
//!
//! * `disarm_power_throttling`  → kills EcoQoS on the QEMU process so Windows
//!   stops parking its vCPUs on E-cores / clamping clocks while the window is
//!   occluded ("idle pre-warming" must not mean "throttled to death").
//! * `pin_to_fastest_cores`     → GetSystemCpuSetInformation + EfficiencyClass
//!   picks the P-core CPU-set ids; SetProcessDefaultCpuSets applies to every
//!   current and future thread (vCPU threads spawn after our call).
//! * `set_priority`             → above-normal while an app runs, idle when not.
//! * `prevent_sleep`            → ES_SYSTEM_REQUIRED while the VM is up.

use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
};
use windows::Win32::System::SystemInformation::{
    GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows::Win32::System::Threading::{
    OpenProcess, SetPriorityClass, SetProcessDefaultCpuSets, SetProcessInformation,
    ABOVE_NORMAL_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
    PROCESS_ACCESS_RIGHTS, PROCESS_INFORMATION_CLASS, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA,
};

use winapk_core::{info, warn};

const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<HANDLE> {
    unsafe { OpenProcess(access, BOOL(0), pid).ok() }
}

const TUNE_ACCESS: PROCESS_ACCESS_RIGHTS =
    PROCESS_ACCESS_RIGHTS(PROCESS_SET_INFORMATION.0 | PROCESS_SET_QUOTA.0 | PROCESS_QUERY_INFORMATION.0);

/// Turn OFF Windows power throttling (EcoQoS) for `pid`.
/// Requires the PROCESS_POWER_THROTTLING_IGNORE_TIMER? No — we set the
/// explicit EXECUTION_SPEED control mask, which wins over the heuristic.
pub fn disarm_power_throttling(pid: u32) -> bool {
    let Some(h) = open(pid, TUNE_ACCESS) else {
        return false;
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0, // 0 = throttling off
    };
    let ok = unsafe {
        SetProcessInformation(
            h,
            PROCESS_INFORMATION_CLASS(4), // ProcessPowerThrottling
            &state as *const _ as *const _,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(h);
    }
    if !ok {
        warn!("SetProcessInformation(PowerThrottling) failed for pid {pid}");
    }
    ok
}

/// CPU-set ids of the fastest core cluster (highest EfficiencyClass).
/// Returns [] on homogeneous systems — caller treats that as no-op.
pub fn fastest_cpu_set_ids() -> Vec<u32> {
    unsafe {
        let mut needed = 0u32;
        let _ = GetSystemCpuSetInformation(None, 0, &mut needed, None, 0);
        if needed == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; needed as usize];
        let mut got = 0u32;
        if !GetSystemCpuSetInformation(
            Some(buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION),
            needed,
            &mut got,
            None,
            0,
        )
        .as_bool()
        {
            return Vec::new();
        }
        let mut sets: Vec<(u32, u8)> = Vec::new(); // (id, efficiency)
        let mut off = 0usize;
        while off < got as usize {
            let info = &*(buf.as_ptr().add(off) as *const SYSTEM_CPU_SET_INFORMATION);
            let cs = info.Anonymous.CpuSet;
            sets.push((cs.Id, cs.EfficiencyClass));
            off += info.Size as usize;
        }
        let Some(max_eff) = sets.iter().map(|s| s.1).max() else {
            return Vec::new();
        };
        let ids: Vec<u32> = sets
            .iter()
            .filter(|s| s.1 == max_eff)
            .map(|s| s.0)
            .collect();
        // Homogeneous → every cpu set is "fastest"; pinning then is pointless.
        if sets.iter().all(|s| s.1 == max_eff) {
            Vec::new()
        } else {
            ids
        }
    }
}

/// Pin all present+future threads of `pid` onto the P-core cluster.
/// Returns true if a restriction was actually applied.
pub fn pin_to_fastest_cores(pid: u32) -> bool {
    let ids = fastest_cpu_set_ids();
    if ids.is_empty() {
        return false;
    }
    let Some(h) = open(pid, TUNE_ACCESS) else {
        return false;
    };
    let ok = unsafe { SetProcessDefaultCpuSets(h, Some(ids.as_slice())) }.as_bool();
    unsafe {
        let _ = CloseHandle(h);
    }
    if !ok {
        warn!("SetProcessDefaultCpuSets failed for pid {pid}");
    }
    ok
}

/// On homogeneous-EfficiencyClass systems (AMD!), P-vs-E detection is
/// blind — but Ryzen X3D chips have two CCDs where only one carries
/// 3D V-Cache. Enumerate L3 caches via RelationCache and return the
/// cpu-set ids inside the largest-L3 group (the gaming CCD).
#[cfg(windows)]
fn x3d_cache_cpu_set_ids() -> Vec<u32> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationCache,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    unsafe {
        let mut needed = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationCache, None, &mut needed);
        if needed == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; needed as usize];
        let mut got = needed;
        if GetLogicalProcessorInformationEx(
            RelationCache,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut got,
        )
        .is_err()
        {
            return Vec::new();
        }
        // Biggest L3 entry → the union of its group masks is the X3D CCD.
        let mut best: Option<(u32, Vec<windows::Win32::System::SystemInformation::GROUP_AFFINITY>)> =
            None;
        let mut off = 0usize;
        while off < got as usize {
            let info = &*(buf.as_ptr().add(off)
                as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            let c = info.Anonymous.Cache;
            if c.Level == 3 && (best.is_none() || c.CacheSize > best.as_ref().unwrap().0) {
                let masks = std::slice::from_ref(&c.Anonymous.GroupMask).to_vec();
                best = Some((c.CacheSize, masks));
            }
            off += info.Size as usize;
        }
        let Some((_, masks)) = best else { return Vec::new() };
        // All L3 entries identical size → no asymmetric cache, nothing to do.
        // (Caller verifies: pinning when every core shares the same L3 is a no-op.)
        let ids = cpu_set_ids_in_groups(&masks);
        // If that covers every cpu set, the cache isn't asymmetric — bail.
        if ids.len() == all_cpu_set_ids().len() {
            Vec::new()
        } else {
            ids
        }
    }
}

#[cfg(windows)]
fn all_cpu_set_ids() -> Vec<u32> {
    each_cpu_set().map(|(id, _, _)| id).collect()
}

/// (group, lp_index) → cpu-set ids whose logical processor sits inside
/// any of `masks`.
#[cfg(windows)]
fn cpu_set_ids_in_groups(
    masks: &[windows::Win32::System::SystemInformation::GROUP_AFFINITY],
) -> Vec<u32> {
    each_cpu_set()
        .filter(|(_, group, lp)| {
            masks.iter().any(|m| m.Group == *group && (m.Mask >> lp) & 1 == 1)
        })
        .map(|(id, _, _)| id)
        .collect()
}

/// Every cpu set on the box as (id, group, logical_processor_index).
#[cfg(windows)]
fn each_cpu_set() -> impl Iterator<Item = (u32, u16, u8)> {
    let mut sets = Vec::new();
    unsafe {
        let mut needed = 0u32;
        let _ = GetSystemCpuSetInformation(None, 0, &mut needed, None, 0);
        if needed != 0 {
            let mut buf = vec![0u8; needed as usize];
            let mut got = 0u32;
            if GetSystemCpuSetInformation(
                Some(buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION),
                needed,
                &mut got,
                None,
                0,
            )
            .as_bool()
            {
                let mut off = 0usize;
                while off < got as usize {
                    let info =
                        &*(buf.as_ptr().add(off) as *const SYSTEM_CPU_SET_INFORMATION);
                    let cs = info.Anonymous.CpuSet;
                    sets.push((cs.Id, cs.Group, cs.LogicalProcessorIndex));
                    off += info.Size as usize;
                }
            }
        }
    }
    sets.into_iter()
}

/// Best-effort preferred cluster: Intel P-cores by EfficiencyClass,
/// else AMD X3D CCD by L3 size. Empty = homogeneous, don't restrict.
pub fn preferred_cpu_set_ids() -> Vec<u32> {
    let ids = fastest_cpu_set_ids();
    if !ids.is_empty() {
        return ids;
    }
    #[cfg(windows)]
    {
        x3d_cache_cpu_set_ids()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Find every `qemu-system-*.exe` descendant of the emulator launcher
/// and pin ITS threads (process default covers future spawns too).
/// Called post-boot — at CreateProcess time the vCPU threads and even
/// the qemu process itself don't exist yet.
#[cfg(windows)]
pub fn repin_guest_threads(launcher_pid: u32) {
    let ids = preferred_cpu_set_ids();
    if ids.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        // qemu may still be exec'ing — retry for ~2 min.
        for _ in 0..24 {
            let qps = qemu_pids_under(launcher_pid);
            if !qps.is_empty() {
                for qp in qps {
                    disarm_power_throttling(qp);
                    if let Some(h) = open(qp, TUNE_ACCESS) {
                        unsafe {
                            let _ = SetProcessDefaultCpuSets(h, Some(ids.as_slice()));
                            let _ = CloseHandle(h);
                        }
                    }
                    pin_existing_threads(qp, &ids);
                    info!("qemu pid {qp}: vCPU threads pinned to preferred cpu sets");
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

#[cfg(not(windows))]
pub fn repin_guest_threads(_: u32) {}

/// Descendants of `launcher_pid` named qemu-system-*.
#[cfg(windows)]
fn qemu_pids_under(launcher_pid: u32) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut e = PROCESSENTRY32W::default();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut rows: Vec<(u32, u32, String)> = Vec::new();
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let name = String::from_utf16_lossy(
                    &e.szExeFile[..e
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(e.szExeFile.len())],
                );
                rows.push((e.th32ProcessID, e.th32ParentProcessID, name));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let mut out = Vec::new();
        for (pid, ppid, name) in &rows {
            if !name.to_ascii_lowercase().starts_with("qemu-system") {
                continue;
            }
            // walk ancestors — catches re-exec'd launchers too
            let mut cur = *ppid;
            for _ in 0..8 {
                if cur == launcher_pid {
                    out.push(*pid);
                    break;
                }
                match rows.iter().find(|r| r.0 == cur) {
                    Some(r) => cur = r.1,
                    None => break,
                }
            }
        }
        out
    }
}

#[cfg(windows)]
fn pin_existing_threads(pid: u32, ids: &[u32]) {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32,
        TH32CS_SNAPTHREAD,
    };
    use windows::Win32::System::Threading::{
        OpenThread, SetThreadSelectedCpuSets, THREAD_SET_INFORMATION,
        THREAD_SET_LIMITED_INFORMATION,
    };
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut te = THREADENTRY32::default();
        te.dwSize = size_of::<THREADENTRY32>() as u32;
        if Thread32First(snap, &mut te).is_ok() {
            loop {
                if te.th32OwnerProcessID == pid {
                    if let Ok(t) = OpenThread(
                        THREAD_SET_INFORMATION | THREAD_SET_LIMITED_INFORMATION,
                        false,
                        te.th32ThreadID,
                    ) {
                        let _ = SetThreadSelectedCpuSets(t, ids);
                        let _ = CloseHandle(t);
                    }
                }
                if Thread32Next(snap, &mut te).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

pub fn set_priority(pid: u32, high: bool) {
    let Some(h) = open(pid, TUNE_ACCESS) else {
        return;
    };
    let class = if high {
        ABOVE_NORMAL_PRIORITY_CLASS
    } else {
        IDLE_PRIORITY_CLASS
    };
    unsafe {
        let _ = SetPriorityClass(h, class);
        let _ = CloseHandle(h);
    }
}

/// Keep the host awake while a VM runs (screen may still turn off).
pub fn prevent_sleep(on: bool) {
    unsafe {
        let _ = SetThreadExecutionState(if on {
            EXECUTION_STATE(ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0)
        } else {
            ES_CONTINUOUS
        });
    }
}
