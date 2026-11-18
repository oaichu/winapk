# WINapk

Android apps as native Windows windows — a Rust/C++ rewrite of the
`RoutineMiku/apkstart` (MIT) model: double-click a `.apk`, it installs,
gets a desktop shortcut, and runs like a native app.

**vs upstream**: no Electron (native orchestrator), GPU passthrough by
default (`-gpu host → angle_indirect → swiftshader`), ART AOT `speed`
compile on install, EcoQoS disabled + P-core pinning, virtio-serial agent
channel instead of ADB-everything, sparse 64 GB userdata.

## Build

```bat
cargo build --release          :: Rust workspace (7 crates → winapk.exe)
native\winapk-display-host\build.cmd   :: C++ display host (MinGW g++)
```

## Use

```bat
winapk provision               :: download emulator+sysimg once (~1.2 GB)
winapk install app.apk         :: parse → install → AOT → desktop shortcut
winapk run com.foo.bar         :: launch in a Windows window
winapk doctor                  :: accel/GPU/runtime diagnostics
winapk assoc on                :: double-click .apk → install
winapk share push|pull         :: %LOCALAPPDATA%\WINapk\share ↔ /sdcard/WINapkShare
winapk config set cores 8
winapk stop
```

Docs: `docs/spec.md` (architecture + roadmap), `docs/agent-build.md`
(guest agent for the virtio-serial channel).

Requires: Windows 10/11 with WHPX or Google AEHD, MinGW g++ for the
display host, Rust. Runtime downloads from `dl.google.com`.
