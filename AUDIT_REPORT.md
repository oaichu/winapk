# Omni-Audit Report — WINapk

- **Ngày:** 2026-11-18
- **Phạm vi:** `C:\Users\HOME\CODE\WINapk` (mã nguồn) + `E:\WINapk` (data/runtime thực tế — logs, screenshots, store)
- **Stack:** Rust workspace 7 crates (~6.5k LOC) + C++ display host MinGW (~450 LOC) + PowerShell scripts
- **Chế độ:** `--scan-only` (đánh giá, không sửa code)
- **Tools run:** `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, grep secret/TODO scan, phân tích 100% mã nguồn thủ công
- **Tools unavailable:** `cargo-audit` (chưa cài) — bù bằng review thủ công Cargo.lock (101 crates, tất cả direct deps ở phiên bản hiện hành: zip 2.6.1, image 0.25.10, clap 4.6.8, serde 1.0.229, windows 0.58, winreg 0.52, toml 0.8.23, thiserror 2.0.21, anyhow 1.0.104 — không CVE đã biết cho các phiên bản này)

## Tóm tắt

| Mức | Tìm thấy | Đã fix | Còn lại |
|-----|----------|--------|---------|
| P0  | 0        | —      | 0       |
| P1  | 4        | 0 (scan-only) | 4 |
| P2  | 8        | 0      | 8       |
| P3  | 10       | 0      | 10      |

**Điểm tổng hợp:** Mã nguồn **8/10** · Hiệu năng **6.5/10** · Giao diện **7/10** · Tính năng **7/10**.
Dự án 2 commits nhưng trưởng thành bất thường so với tuổi: kiến trúc sạch, comment domain xuất sắc, không TODO nợ, không secret, không spaghetti. Các lỗi tìm thấy tập trung ở **edge-case parsing APK**, **persistence không atomic** và **2 stub tính năng**.

## Kết quả kiểm chứng

- Build: **PASS** (7 crates compile sạch; C++ host build riêng qua MinGW)
- Tests: **9/9 pass** — nhưng độ phủ cực mỏng: 3 test FSM + 1 env-gated seed + 5 test `pick_abi`. **0 test** cho AXML/ARSC parser, ext4 builder, control protocol, share, install pipeline
- Clippy `-D warnings`: **FAIL — đúng 1 lint** toàn workspace: `double_ended_iterator_last` tại `crates/winapk-core/src/adb.rs:267` (`.last()` → `.next_back()`, thuần style)
- Secret scan: sạch. TODO/FIXME/HACK: 0

---

## Chi tiết P1 (nên sửa trước khi release)

### P1-1. store.json: ghi non-atomic + load nuốt lỗi = mất sạch danh sách app
- `crates/winapk-install/src/store.rs:33-38` — `save()` dùng `fs::write` trực tiếp; crash/disk-full giữa chừng để lại file cụt (log thực tế `E:\WINapk\logs\winapk.log:19,167` cho thấy disk-full ĐÃ xảy ra trên máy này).
- `store.rs:26-31` — `load()` là `unwrap_or_default()`: JSON hỏng → trả list rỗng **im lặng** → lần install kế tiếp ghi đè trắng, mất toàn bộ record.
- **Fix đề xuất:** write-to-temp + `rename`, và khi parse lỗi thì rename file lỗi sang `store.json.bak` + warn, không trả default.

### P1-2. avd.rs ghi `hw.sensors.orientation` hai lần — giá trị sau thắng, mâu thuẫn chính comment của nó
- `crates/winapk-engine/src/avd.rs:80` ghi `"no"` kèm comment "enabled it auto-rotates the whole display and wrecks freeform window layout (upstream battle damage)", rồi `avd.rs:90` ghi `"yes"` (trong nhóm sensors mặc định). AVD config parse last-wins → orientation = **yes**, tái đưa đúng bug mà dòng 80 tuyên bố đã tránh.
- **Fix:** bỏ dòng 90 (hoặc tách nhóm sensors ra skip orientation).

### P1-3. AXML/ARSC parser panic trên APK malformed (untrusted input)
- `crates/winapk-install/src/axml.rs:96-137` — `read_string_pool`: `count`, `offsets`, `strings_start` lấy thẳng từ file rồi index `buf[...]` qua `u16le/u32le` **không bounds-check** → APK ác ý/corrupt làm `winapk install` panic (index out of range). Tương tự `read_len8` (axml.rs:141-149).
- `axml.rs:176-177` — namespace chunk đọc `pos+16`/`pos+20` khi vòng lặp chỉ guard `pos+8 <= buf.len()` → đọc quá cuối buffer ~8 byte.
- `crates/winapk-install/src/arsc.rs:35,46-49` — cùng class: offset-table và `u16le(buf, p)` khi `p == buf.len()-1`.
- Không memory-unsafe (chỉ panic), nhưng crash-on-input với file tải về là P1 với tool dạng này. **Fix:** mọi read qua helper checked (`buf.get(..)`) như `arsc.rs` đã làm ở `parse_type`/`read_value` (phần đó làm đúng — chuẩn hóa nốt phần còn lại).

### P1-4. WatchExit là stub hai đầu — "app exit" không bao giờ được báo
- Guest agent `crates/winapk-agent/src/main.rs:271` — nhận `WatchExit`, trả `Ok{"watch:pkg"}`, **không làm gì**; không bao giờ gửi `AgentMsg::AppExit`.
- Host `crates/winapk-engine/src/control.rs:148-150` — AdbChannel trả `"watch-adb-poll"` nhưng không có code nào poll.
- Hệ quả: app Android tự thoát/crash → cửa sổ host vẫn treo (chỉ đóng khi emulator chết hoặc user đóng). `docs/spec.md:78` liệt kê "app-exit" trong tính năng agent.

## Chi tiết P2

| # | Vị trí | Vấn đề |
|---|--------|--------|
| P2-1 | `engine/control.rs:92-109` | `VirtioSerialChannel::call` tạo `BufReader` mới mỗi call, không correlation-id — mọi message async (vd AppExit khi P1-4 được hiện thực) sẽ làm lệch request/response stream. Hiện "chạy được" chỉ vì chưa có message async. |
| P2-2 | `winapk-share/src/lib.rs:126-129` | Share **pull N+1**: một `adb shell stat` mỗi file (~100-200ms/spawn). 100 file ≈ 20s. Nên batch: `find . -type f -exec stat -c "%s %n" {} \;` một lần, hoặc `ls -lR` parse. |
| P2-3 | `engine/provision.rs:206-209` | Runtime zips (1.2GB+) tải về không verify SHA-256 (có HTTPS, không pin checksum). Zip hỏng nửa chừng → extract lỗi khó hiểu xa xuống pipeline. |
| P2-4 | `engine/engine.rs:326-336` | `pick_console_port` TOCTOU: bind-probe xong drop port rồi mới spawn emulator; và khi cả 32 port bận thì **im lặng** trả về `base` (có thể đang bận) thay vì báo lỗi. |
| P2-5 | `engine/engine.rs:793` | `self.adb_client().unwrap()` trong `open_channel` — đường adopt + agent-ping fail + adb.exe thiếu → panic thay vì Err. |
| P2-6 | `winapk-share/src/lib.rs:131` | `pull` chỉ so **size**, không mtime (push có manifest size+mtime, pull thì không) — file sửa cùng kích thước không được đồng bộ. |
| P2-7 | agent `main.rs:277-279` + `control.rs:161-164` | `ClipboardGet` trả Err ở cả hai transport — spec liệt kê clipboard là tính năng. |
| P2-8 | Thiếu lệnh **uninstall** | CLI có install/run/list/… nhưng không có `winapk uninstall <pkg>`: .lnk desktop, .ico, record store.json, package trong guest — không có đường dọn nào. Upstream apkstart có. |

## Chi tiết P3

| # | Vị trí | Vấn đề |
|---|--------|--------|
| P3-1 | `core/adb.rs:267` | Lint clippy duy nhất: `.last()` trên DoubleEndedIterator → `.next_back()`. |
| P3-2 | `native/.../main.cpp:122` | FIT_TIMER 100ms gọi `SetWindowPos` mỗi tick kể cả khi client size không đổi — nên cache rect cuối. |
| P3-3 | `engine/engine.rs:745` | deploy_agent idempotency chỉ so size: agent hỏng cùng size không bị re-push. |
| P3-4 | `winapk/main.rs:292-294` | `set_config`: `num(val).unwrap_or(0)` — giá trị rác thành 0 (=auto) im lặng. |
| P3-5 | `winapk/main.rs:174` | Run flow `sleep(2)` cứng rồi đọc freeform_bounds — race với máy chậm/layout muộn. |
| P3-6 | `engine/userdata.rs:268` | ext4 `write_dir_block`: `BLOCK - (off - base)` underflow nếu directory entries tràn 1 block (hiện chỉ vài entry — seed nhiều file sẽ panic). |
| P3-7 | `install/install.rs:111-133` | `order_parts` múa may `ordered_info`/`tail` — hoạt động nhưng rối; dư `ordered_info` biến trung gian. |
| P3-8 | `install/shortcut.rs:38-61` | `CoInitializeEx`/`CoUninitialize` không cân bằng trên early-return `?` — nên dùng RAII guard. |
| P3-9 | `control.rs:155` + agent `main.rs:211` | Clipboard quoting `replace('\'',"")` — mất dữ liệu (xóa dấu nháy đơn khỏi text user). |
| P3-10 | Kiến trúc adb | Mỗi ADB op spawn một `adb.exe` — polling boot tốn 2-3 process/cycle. Pragmatic, overhead chấp nhận được, nhưng là ceiling hiệu năng về sau (nên cân nhắc adb server protocol client). |

## Hiệu năng — bằng chứng từ runtime thực tế (E:\WINapk)

- **Boot lạnh:** 346–716s (5.8–12 phút) trên host Win10 19045 / WHPX / 4 vCPU / 3GB RAM. Lần tốt nhất 346s (`winapk.log:245-249`), gần nhất 387–392s (`winapk.log:277-281, 288-292`). WHPX + cold boot + first-boot restorecon là các yếu tố chính. **Đề xuất:** bật snapshot có kiểm soát hoặc giữ VM ngầm (đã có hạ tầng idle-priority tốt), hoặc giảm lcd 1080x1920 khi boot.
- **GPU:** `-gpu host` passthrough boot thành công và render sạch — screenshots guest (probe*.png: Google splash; shot.png: USB-auth dialog) **không artifact, không tear**. GPU fallback chain đã thực sự cứu vài lần trong log (host fail → angle → swiftshader).
- **Boot-fail loop từng tốn hàng giờ** (log 17949700–17949765): auth-guard QMP tap bấm "Allow" 12 lần/lần-chuẩn-đoán mà không bao giờ thành trên playstore image → đã giải quyết tận gốc bằng google_apis userdebug image + seeded adb_keys (đúng hướng, đáng khen).
- **Install→AOT:** install ~5s, AOT speed-compile async in-guest (không block) — đúng thiết kế.
- **Idle:** tắt hết app → VM hạ priority IDLE — tốt cho máy trạm; điểm cộng lớn so upstream (dump ram.bin GB-scale).
- **Còn chậm:** share pull N+1 (P2-2); polling cycle spawn adb nhiều.

## Giao diện

**Windows-side (display host C++):** PerMonitorV2 DPI set trước khi tạo window (đúng), reparent HWND "làm đúng" — restyle WS_CHILD, re-home khi host đóng (SetParent NULL — crash không mồ côi surface), focus forwarding WM_ACTIVATE, crop theo freeform bounds + zoom-to-fit, icon thật từ APK, title theo label app. Tối giản chủ ý. **Thiếu so upstream:** nhớ kích thước cửa sổ theo app, fullscreen toggle, taskbar grouping per-app (upstream claim có).

**Guest render:** sạch (xem trên).

**CLI UX:** đủ và rõ (install/run/stop/list/doctor/assoc/share/config/inspect); doctor chi tiết thật (accel check + GPU vendor + P-core detect + runtime status).

## Tính năng vs docs/spec.md

| Spec khai báo | Thực tế |
|---|---|
| provision / AVD / launch / GPU chain / accel | ✅ chạy thật, có bằng chứng log |
| FSM + Win32 tuning (EcoQoS, P-core, priority, anti-sleep) | ✅ đầy đủ + X3D CCD bonus (ngoài spec) |
| install: AXML/ARSC → ICO → .lnk → AOT | ✅ (label tiếng Trung resolve đúng: 谷歌拼音输入法) |
| display: reparent + crop; stream = phase 2 | ✅ reparent chạy; stream là khung đúng như khai báo |
| share: adb-sync dirty-track | ✅ push có manifest; pull còn N+1 + size-only |
| agent: ping/am-start/app-exit/clipboard/mount/shutdown | ⚠️ ping/start/stop/mount/shutdown ✅ (virtio-serial channel hoạt động thật, log:293); **app-exit stub (P1-4), clipboard-get stub (P2-7)** |
| CLI 9 lệnh | ✅ + inspect; **thiếu uninstall (P2-8)** |
| AOSP custom / gRPC display / balloon | spec'd phase 2 — không tính |

## Điểm mạnh đáng giữ nguyên (không được phá khi refactor)

1. `tuning.rs` — P/E-core + AMD X3D CCD detection, EcoQoS disarm, KILL_ON_JOB_CLOSE job keeper + orphan sweep match-cmdline: cả một tầng upstream không có.
2. `userdata.rs` — ext4 builder viết tay với FBE encrypt-flag dance + NTFS sparse: domain expertise hiếm, comment ghi lại lý do từng bước.
3. Cross-process named-mutex (`Global\WINapk-vm`) chống race 2 CLI invocation; FSM explicit; zip-slip guard (`enclosed_name`); CREATE_NO_WINDOW everywhere; adb port riêng 5038.
4. `wait_boot` adaptive liveness (log-growth heuristic + granted-time extension) — giải quyết đúng bài toán "fail fast vs slow host".
5. Release profile: lto thin + codegen-units 1 + strip. Dependencies lean (101 locked crates, 0 transitive rác).

## Khuyến nghị hành động (theo thứ tự)

1. **P1-1** store atomic-write + backup-on-corrupt (30 phút).
2. **P1-2** xóa dòng `orientation yes` trùng (5 phút).
3. **P1-3** bounds-check toàn bộ AXML/ARSC reads + thêm 5-10 unit test với blob malformed (nửa ngày).
4. **P1-4 + P2-7** hiện thực WatchExit (poll `pidof` in-guest agent, push AppExit qua channel — cần解 P2-1 correlation trước) + clipboard-get.
5. **P2-8** lệnh `uninstall` (remove .lnk/.ico/store record/`pm uninstall`).
6. **P2-2** batch share pull.
7. Test coverage: ext4 builder round-trip qua `fs_bytes`/`has_ext4_superblock`, protocol host↔agent qua TCP mode (agent đã có `--tcp` test mode sẵn!), AXML/ARSC với APK thật + malformed.
