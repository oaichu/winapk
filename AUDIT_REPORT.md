# Omni-Audit Report

- **Date:** 2026-11-18
- **Stack detected:** Rust workspace (7 crates: winapk, winapk-core, winapk-engine, winapk-install, winapk-display, winapk-share, winapk-agent) + C++17 display host (MinGW) + PowerShell packaging scripts. Toolchain: cargo 1.99, rust-lld (musl agent), makeappx/signtool.
- **Tools run:** `cargo check --all-targets` (0 warnings), `cargo test --all` (17/17 PASS), `cargo clippy --fix` + `cargo clippy` (0 warnings remaining), `cargo check --target x86_64-unknown-linux-musl -p winapk-agent` (PASS), manual secret grep, manual SAST on all untrusted-input sinks.
- **Tools unavailable:** `cargo audit` (not installed — no network install attempted; dep tree is small and pinned in Cargo.lock), `gitleaks`/`semgrep` (not installed — compensated by manual pattern grep + full manual review of every shell-interpolation, parser, IPC and process-lifecycle site listed below).

## Tóm tắt

| Mức | Tìm thấy | Đã fix | Còn lại |
|-----|----------|--------|---------|
| P0  | 0        | 0      | 0       |
| P1  | 13       | 13     | 0       |
| P2  | 7        | 7      | 0       |
| P3  | 8        | 0      | 8 (liệt kê, không xóa) |

## Chi tiết lỗi đã fix

### P1 — High

- **[P1] `crates/winapk-install/src/axml.rs`** — Binary AXML parser panic trên input crafted: `u16le/u32le` index slice trực tiếp (OOB), `Vec::with_capacity(count)` tin `stringCount` từ file → OOM, `buf[p..end]` unguarded, `read_len8` OOB. Fix: total readers (`get` → 0 on OOB), caps `MAX_STRINGS/MAX_STR_LEN/MAX_DEPTH`, `chunk_size < 8` break, `saturating_add`, guarded slices. Tests: `garbage_and_truncation_never_panic`, `hostile_string_pool_count_no_oom`.
- **[P1] `crates/winapk-install/src/arsc.rs`** — Cùng lớp lỗi (resources.arsc parser). Fix tương tự + test `garbage_never_panics`.
- **[P1] `crates/winapk-share/src/lib.rs:124`** — `pull()` join trực tiếp guest-provided `find` names vào host share dir → `../../x` ghi ra ngoài (path traversal từ nội dung guest). Fix: `safe_rel()` chỉ cho `Component::Normal`, reject `\`,`:` , `..`, absolute; `dest.starts_with(dir)` belt-and-braces; refuse ghi qua symlinked dest.
- **[P1] `crates/winapk-share/src/lib.rs`** — Shell injection qua quote `'` trong filename: `mkdir -p '{dir}'`, `stat -c %s '{remote}'`. Fix: `sq()` POSIX-escape `'` → `'\''`.
- **[P1] `crates/winapk-share/src/lib.rs:38`** — `scan()` dùng `p.is_dir()` follow symlink → cycle/escape share dir. Fix: `e.file_type()` (no-follow).
- **[P1] `crates/winapk-core/src/proc.rs:15`** — `run()` poll `try_wait` với stdout/stderr piped → deadlock kinh điển khi child đổ >pipe-buffer (taskkill/powershell đủ lớn để trúng). Fix: drain 2 pipes trên reader threads, join sau wait/timeout-kill.
- **[P1] `crates/winapk-engine/src/control.rs:61`** — `VirtioSerialChannel` tạo `BufReader` mới mỗi call trên cloned socket → byte đã buffer bị mất → desync framing; async `AppExit` lẫn vào reply. Fix: persistent BufReader; read-loop bỏ qua `AppExit` events (log) cho tới khi gặp reply.
- **[P1] `crates/winapk-engine/src/engine.rs:287`** — `adopt_running()` chỉ kiểm `pid_alive` + serial boot=1 đầu tiên → có thể adopt emulator của người dùng (emu kill giết nhầm VM) hoặc taskkill pid bị reuse. Fix: đọc `process_cmdline(pid)`, bắt buộc chứa AVD tag `WINapk` + derive serial từ `-port N` của chính cmdline đó; không derive được → refuse adopt.
- **[P1] `crates/winapk-engine/src/engine.rs:851`** — `stop()` nhánh adopted thiếu `kill_emulator_orphans` → launcher chết còn QEMU detached sống sót (đúng bug sweep được viết để xử). Fix: sweep sau kill_tree + guard `process_cmdline` trước taskkill.
- **[P1] `crates/winapk-agent/src/main.rs`** — Guest agent interpolate `pkg`/`activity`/`tag`/`target` thẳng vào `sh -c` — bất kỳ process local nào connect vào QEMU chardev TCP 45678 đều inject được guest command as root. Fix: `valid_token()` charset whitelist `[a-zA-Z0-9._\-/@:$]` + len ≤512, target bắt buộc absolute + cấm `..`; line cap 1MiB trong `serve()`.
- **[P1] `crates/winapk-agent/src/main.rs:209` + `control.rs:152`** — Clipboard `text.replace('\'',"")` xóa quote (sai escape → vẫn injectable qua `'; ...`). Fix: POSIX `'\''` escape cả hai phía.
- **[P1] `native/winapk-display-host/src/present_d3d11.cpp`** — D3D11 immediate context race: worker `Map/CopyResource/Present` vs UI-thread `ClearState/ResizeBuffers` → UB. Fix: `SRWLOCK` bảo vệ mọi ctx/swap access.
- **[P1] `crates/winapk-install/src/store.rs` + `install.rs`** — `store.json` lost-update race giữa 2 process install đồng thời + ghi file không atomic (crash → JSON cụt → "no apps"). Fix: `Global\WINapk-store` named mutex quanh load→upsert→save + tmp+rename.

### P2 — Medium

- **[P2] `crates/winapk-install/src/apk.rs:37`** — `Vec::with_capacity(f.size())` tin zip-reported size → OOM claim 4GB. Fix: cap prealloc 64MB + `take(256MB)` read cap.
- **[P2] `crates/winapk-install/src/apk.rs:156`** — `<uses-sdk>` tìm nhầm trong `<application>` → `min_sdk` luôn `None`. Fix: lookup trực tiếp trên `<manifest>` root.
- **[P2] `crates/winapk-install/src/apk.rs:65`** — `package` name từ manifest đi vào shell commands (`am force-stop`, `pm path`, AOT). Fix: charset `[a-zA-Z0-9._]`, len ≤256 — reject crafted manifest sớm nhất.
- **[P2] `crates/winapk-install/src/install.rs:97`** — `expand_package` dùng `rsplit('/')` — zip entry `a\..\evil.apk` (`\` là separator trên Windows) thoát temp dir; không có giới hạn tổng size giải nén (zip bomb). Fix: `rsplit(['/','\\'])` + reject `.`/`..` + cumulative cap 4GB.
- **[P2] `crates/winapk-engine/src/engine.rs:783`** — `open_channel()` `unwrap()` adb client → panic nếu adb.exe mất. Fix: graceful fallback giữ virtio channel.
- **[P2] `crates/winapk-engine/src/userdata.rs`** — File seed >12 direct inode blocks (48KB) ghi đè inode fields; dir entries tràn 1 block → corrupt image âm thầm. Fix: `assert!` contract (48KB/file, name ≤255B, children fit 4KiB block).
- **[P2] `native/winapk-display-host/src/main.cpp:150`** — `DEAD_TIMER` `dead || !pr` — `OpenProcess` fail thoáng (ACL/race) → cửa sổ app tự đóng. Fix: chỉ destroy khi `GetExitCodeProcess` xác nhận `!= STILL_ACTIVE`. + `LocalFree(argv)` leak.
- **[P2] `native/.../present_d3d11.cpp`** — `hdr->width/height/stride` từ shared memory không validate → memcpy OOB trong mapped view. Fix: clamp ≤8192, `stride ≥ w*4`, total <2GB.

### Clippy style (auto-fixed)
- borrowed-expression, manual `div_ceil`, manual range contains, `unwrap` sau `is_some`, field-reassign-with-default, manual clamp, test-module ordering — ~25 sites qua `cargo clippy --fix` + 4 fix tay. **0 warnings remaining.**

## Cảnh báo bảo mật

**True positives đã fix:**
- Share-pull path traversal (guest → host write escape) — mức nặng nhất trong batch.
- Agent channel command injection (host-local → guest root shell) — sanitizer charset.
- Crafted-APK parser OOB/OOM panics — DoS trên mọi `install`.
- Adopt/kill foreign emulator + pid-reuse taskkill — an toàn người dùng.
- `.xapk` member-name traversal (`\` separator) + zip-bomb.
- store.json race/corruption; pipe deadlock; clipboard/`'`-escape injection.

**Residual risk (ghi nhận, không fix trong lượt này):**
- QEMU chardev TCP `127.0.0.1:agent_port` không auth — bất kỳ process local nào vẫn *gửi* lệnh được, nhưng sanitizer giới hạn còn whitelist tokens + ops cố định. Khử triệt để cần handshake token (phase `winapkd`).
- `WatchExit` vẫn là stub — host chưa poll exit thật.
- Adopt sweep dựa trên AVD tag `WINapk` — ai đó đặt AVD cùng tên vẫn bị sweep (thiết kế chấp nhận: tag là identity).

## Tech debt & Dead code (chưa xóa, cần review)

| Item | Mức tin cậy |
|---|---|
| `userdata::build_empty_ext4` — wrapper back-compat, không caller | Cao — safe để xóa |
| `accel::hyperv_available` — không caller | Cao |
| `provision::zip_entry_bytes` — không caller (apk.rs có bản riêng) | Cao |
| `adb::aot_compile` — thay bởi async AOT in-guest | Cao |
| `display::alive`, `emulator_pid_hint` — không caller | Cao |
| `AgentCmd::MountShare` — protocol có, host chưa bao giờ gửi (chờ 9p/virtio-fs) | Spec'd future — giữ |
| `WatchExit` — stub trả Ok giả | Feature incomplete — giữ + TODO |
| `shortcut.rs`/`icon.rs` clippy auto-fixes — chỉ style | — |

Unused dependencies: không phát hiện (tất cả deps trong 7 Cargo.toml đều có import; `libc` chỉ dùng cho linux target — đúng thiết kế).

## Kết quả kiểm chứng

- **Type check:** `cargo check --all-targets` PASS (0 warnings)
- **Lint:** `cargo clippy --all-targets` PASS (0 warnings — trước ~21)
- **Tests:** 17/17 PASS (5 existing ABI + 12 mới: parser fuzz-lite, safe_rel, sq, valid_token, mount rules)
- **Cross-target:** `x86_64-unknown-linux-musl` agent check PASS
- **Live regression (2026-11-18, post-fix):** PASS — cold boot→`run` qua virtio-serial agent (persistent reader) → window attached; process khác `doctor`/`share` adopt đúng `emulator-5554` qua cmdline `-port` pin; `share pull` batch-stat chạy sạch; `stop` (adopted-path) dọn 0 process sót. Fallback `adb` khi chardev đã bị session đầu giữ — đúng thiết kế (single-connection).
