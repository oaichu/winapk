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

use winapk_core::warn;

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
