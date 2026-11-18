# WINapk — Architecture Spec

WINapk là bản fork/viết lại `RoutineMiku/apkstart` (MIT) theo hướng "chất lượng
biên dịch cao, ổn định dài hạn, vắt kiệt phần cứng x86". Giữ nguyên UX nguồn:
**double-click .apk → cài → shortcut desktop → app mở như phần mềm Windows gốc**;
toàn bộ tầng dưới (Electron orchestrator, SetParent hack, ADB-everything, GPU
software fallback) được thay thế.

## Hiện trạng apkstart (đã audit trên source)

- Orchestrator: Electron + ~20k dòng JS (`engine.js` 1591, `emulator.js` 1677…)
- GPU mặc định `software` (emulator.js:391), Vulkan backend bị cắt khỏi runtime
- Window: Win32 `SetParent` nhét emulator HWND vào Electron window → chung input
  queue (AttachThreadInput), DPI-lệch, phải chạy vòng lặp giấu conhost/netsimd
- Tài nguyên: clamp cores 2–4, RAM 2–4 GB, userdata 4 GB; RAM dump `ram.bin`
- Mọi kênh điều khiển + đồng bộ file đều vòng qua `adb.exe` (polling)
- Model chạy: **1 shared VM**, app mở freeform (`am start --windowingMode 5`),
  mỗi host window crop emulator screen vào đúng freeform bounds của app

## Kiến trúc đích

```
winapk.exe (CLI/shell thay Electron)
├── winapk-core      config / paths / adb client / process utils
├── winapk-engine    provision + AVD + launch + lifecycle FSM + Win32 tuning
├── winapk-install   APK parse (AXML/ARSC) + install + AOT + shortcut
├── winapk-display   spawn/display-host manager (Rust) 
│     └── native/winapk-display-host  C++17 Win32+D3D11 (g++/MinGW)
├── winapk-share     file share (adb-sync now, virtio-fs when image có)
└── winapk-agent     guest agent (Android ELF, virtio-serial channel)
```

Runtime vẫn là **Google Android Emulator (QEMU + WHPX/AEHD) + image
`android-34 google_apis_playstore x86_64`** provision từ `dl.google.com`.
Custom AOSP là phase sau — hooks đã chừa sẵn (`runtime/manifest.json`).

### Tầng 1 — Biên dịch & dịch mã

- `-prop` inject tại launch: `dalvik.vm.isa.x86_64.variant`, `…features` để
  ART/dex2oat sinh mã AVX2/FMA/BMI2 (x86-64-v3) — config `[props]` bật/tắt.
- Sau `adb install`: `cmd package compile -m speed -f <pkg>` (AOT full).
- Install ưu tiên `--abi x86_64` nếu APK có `lib/x86_64/` (tránh dịch mã).
- Phase sau: custom image cho phép `libhoudini` cạnh `libndk_translation`,
  per-app translator profile.

### Tầng 2 — Đồ họa & cửa sổ

- GPU fallback chain: `host → angle_indirect → swiftshader_indirect` (detect
  qua GPU vendor + `-accel-check` + lần boot trước có crash không).
- Vulkan/gfxstream giữ nguyên trong runtime (không slim-cut như upstream).
- Display host C++: PerMonitorV2 window + D3D11 swapchain; 2 transport:
  - `reparent` (hiện tại): SetParent emulator HWND **làm đúng** — children
    spawn `CREATE_NO_WINDOW` (hết cửa sổ conhost), title-bar riêng, crop
    vào freeform bounds của app, re-home HWND theo focus.
  - `stream` (đã dựng khung): BGRA frame qua named shared-memory → present
    bằng DXGI — cắm gRPC screenshot/gfxstream surface là phase 2.
- Phase 2: virtio-gpu multi-scanout → display-id per app, `am start --display`.

### Tầng 3 — Ảo hóa / CPU / RAM

- Accel detect: `emulator -accel-check` + registry `Services\gvm` →
  `whpx | aehd | tcg(off)`; `-smp`/`hw.ramSize`/`disk.dataPartition.size`
  từ `EngineProfile` (mặc định 6 cores / 6 GB / 64 G sparse qcow2).
- Win32 tuning (sau spawn):
  - `SetProcessInformation(ProcessPowerThrottling)` → tắt EcoQoS throttling
  - `GetSystemCpuSetInformation` → EfficiencyClass → `SetProcessDefaultCpuSets`
    ghim vCPU threads vào P-core (hybrid detect; homogeneous = no-op)
  - `SetPriorityClass(ABOVE_NORMAL)` khi active / `IDLE` khi không có app
  - `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` chống sleep lúc VM chạy
- Thay quickboot `ram.bin`: quickboot vẫn optional; mặc định `-no-snapshot`
  + idle-đóng app (không ghi GB xuống ổ). Phase 2: virtio-balloon reclaim.

### Tầng 4 — Host↔Guest

- Control channel chính: **virtio-serial** (`-chardev socket` + named
  `virtserialport winapk.0` → `/dev/virtio-ports/winapk.0`). Hoạt động trên
  mọi accelerator kể cả WHPX — khác virtio-vsock vốn cần vhost (Linux-only).
- `winapk-agent` (Rust ELF chạy trong guest): ping / am-start / app-exit /
  clipboard / mount / shutdown, JSON line protocol.
- Fallback không cần custom image: ADB channel (cùng protocol, transport
  `adb shell` + `adb forward`).
- File share: `adb push --sync` + manifest dirty-tracking (size+mtime) ở
  `%LOCALAPPDATA%\WINapk\share`; phase 2 `-virtfs`(9p)/virtiofsd mount.

### Tầng 5 — Nợ kỹ thuật & custom AOSP

- Lifecycle = FSM (`Off→Provisioned→Launching→Booting→Ready→AppRunning→
  Stopping→Off`, `Failed` từ mọi state) — thay callback-spaghetti engine.js.
- `runtime/manifest.json` trỏ system/vendor image → đổi sang AOSP tự build
  (Clang LTO `-march=x86-64-v3`, MGLRU, zram lz4, bỏ GMS) = chỉ đổi manifest.
- Phase AOSP cần Linux + ~200 GB disk; spec-only trong milestone này.

## Session 1 deliverables (đã implement)

| Thành phần | Trạng thái |
|---|---|
| Cargo workspace 7 crates + C++ host | scaffold hoàn chỉnh |
| Engine: provision (platform-tools/emulator/sysimg/AEHD từ dl.google.com) | full |
| Engine: AVD config + launch args + GPU chain + accel detect | full |
| Engine: FSM + Win32 tuning (EcoQoS/P-core/priority) | full |
| Install: AXML/ARSC port, icon→ICO, `adb install --abi`, AOT speed, .lnk | full |
| Display host: reparent-done-right + crop freeform + stream-transport khung | real, stream-source=phase2 |
| Share: adb-sync dirty-track + virtio-fs config hook | adb real / vfs spec |
| Agent: Rust android ELF + virtio-serial/TCP protocol | code + docs/agent-build.md |
| CLI: install/run/stop/list/doctor/assoc/share/config/provision | full |
| Custom AOSP build, gRPC display, libhoudini, virtio-balloon | spec'd, phase 2 |

## CLI

```
winapk provision            # tải runtime lần đầu (~1.2 GB)
winapk install app.apk      # parse → adb install --abi → AOT → shortcut
winapk run <pkg>            # boot VM (nếu cần) → am start → host window
winapk stop|list|doctor     # vận hành
winapk assoc on|off         # đăng ký .apk file association (HKCU)
winapk share push|pull      # đồng bộ thư mục share
winapk config set k v       # cores/ram_mb/gpu_mode/userdata_gb/props
```

## Layout ổ cứng

```
%LOCALAPPDATA%\WINapk\
├── config.toml
├── runtime\{emulator,platform-tools,system-images,aehd}\…
├── avd\WINapk.avd\ + WINapk.ini   (ANDROID_AVD_HOME=avd\)
├── share\        ↔ /sdcard/WINapkShare
├── icons\<pkg>.ico
├── logs\
└── store.json    (installed apps)
```
