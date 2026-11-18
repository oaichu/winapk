# Building & deploying winapk-agent

The agent runs **inside the Android guest** on `/dev/virtio-ports/winapk.0`
(virtio-serial) — QEMU side is created at launch time (`-chardev socket` +
`-device virtserialport,name=winapk.0`, see `winapk-engine/src/engine.rs`).

Two artifacts:

1. **Host test binary** (`x86_64-pc-windows-*`) — `cargo build -p winapk-agent`
   then `winapk-agent --tcp 45678`. The engine's `VirtioSerialChannel`
   connects to it exactly as it would inside the guest: same JSON-lines
   protocol, so the whole channel is testable on Windows.

2. **Guest binary** — needs an Android target + NDK linker:

```powershell
rustup target add x86_64-linux-android
# SDK NDK must be installed (sdkmanager "ndk;26.x" or side-load)
$env:CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER = "$env:ANDROID_NDK_HOME\toolchains\llvm\prebuilt\windows-x86_64\bin\x86_64-linux-android34-clang.cmd"
cargo build -p winapk-agent --target x86_64-linux-android --release
adb -s emulator-5554 push target\x86_64-linux-android\release\winapk-agent /data/local/tmp/
adb -s emulator-5554 shell "chmod +x /data/local/tmp/winapk-agent && /data/local/tmp/winapk-agent &"
```

For a persistent setup without rebuilding the image, drop a wrapper into
the AVD's `init` via `adb remount` + `init.winapk.rc`, or bake it into a
custom system image (`runtime/manifest.json` → `sysimg_dir`).

## Protocol (JSON lines, one msg per line)

Request (host→guest), `op` snake_case:
`ping | start{pkg,activity?,freeform} | stop{pkg} | watch_exit{pkg} |
clipboard_set{text} | clipboard_get | mount_share{tag,target} | shutdown`

Reply (guest→host), `type` snake_case:
`pong{agent_version} | ok{op} | err{op,msg} | app_exit{pkg} | clipboard{text}`

The Rust enum definitions are duplicated verbatim between
`crates/winapk-engine/src/control.rs` (host) and
`crates/winapk-agent/src/main.rs` (guest) — keep them in sync.
