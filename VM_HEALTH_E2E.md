# VM Health Check — E2E (2026-11-18)

> **CẬP NHẬT cuối ngày (đợt phản biện):** kết luận "install → crash" đã được kiểm chứng lại bằng A/B trên VM live — xem mục cuối "Phản biện & chẩn đoán sửa".

## Trạng thái hiện tại: CHẠY ĐƯỢC nhưng có 1 bug nghiêm trọng chưa fix

- emulator-5554: authorized, system_server ổn định (pid mới từ 18:34), agent virtio-serial chạy (pid 2021)
- PinyinIME đã cài thành công, AOT queued
- emulator.log của boot này: chỉ warning vô hại (Qt overlay)

## BUG chính: system_server crash mỗi lần cài app

**Dropbox có 8 crash** (`/data/system/dropbox/system_server_crash@*.txt`), chia 2 loại:

| Loại | Khi nào | Nguyên nhân | Trạng thái |
|---|---|---|---|
| Phase-500 NMS boot crash | 3 lần, sớm hôm nay (trước restorecon fix) | SELinux label sai ở userdata seed | **Đã fix** bằng cơ chế `.needs-restorecon` của engine |
| **NPE backup service** | 5 lần, mỗi lần `winapk install` | `PACKAGE_ADDED` → `UserBackupManagerService.dequeueFullBackupLocked` NPE (mFullBackupQueue null) → framework chết + restart 30–60s, mọi app DeadSystemException, GMS ANR | **Chưa fix** |

Đã thử repair trên VM live:
- `bmgr enable true` → **cũng crash** (scheduleNextFullBackupJob NPE cùng queue null)
- `bmgr init com.google.android.gms/.backup.BackupTransportService` → `-1000` (không init được)
- `/data/backup/` chỉ có `backup_enabled` + 3 dir transport, thiếu state queue

→ Backup manager của image google_apis r14 (API-34) này ở trạng thái hỏng bền vững; không repair được từ host.

## Tác động

- Cài app vẫn THÀNH CÔNG (adb trả Success trước khi broadcast crash), nhưng sau mỗi lần cài:
  - framework restart ~30–60s (mất thêm ~1 phút)
  - toàn bộ app nền chết + khởi động lại; GMS apps ANR dồn cục trên host yếu
  - rủi ro (thấp nhưng có) hỏng dữ liệu lúc framework chết giữa chừng

## Đường fix

1. **(Nhanh nhất, cần test)** Đổi sysimg revision: manifest trỏ về `x86_64-34_r10` (fallback cũ của provision.rs) hoặc đợi bản mới hơn r14 → re-provision + reset userdata. Chưa chắc r10 hết bug — phải test.
2. **(Đúng roadmap phase-2)** Custom AOSP image — sửa/gỡ BackupManagerService khỏi image, kiểm soát trọn vẹn.
3. **(Mitigation winapk-side, làm ngay được)** Accept crash như hiện trạng nhưng: sau `adb install`, engine nên đợi framework sống lại (poll `sys.boot_completed`/AMS ready ~60s) rồi mới AOT + icon + shortcut, tránh việc AOT queue bị nuốt trong window restart. Hiện chạy "trườn qua được" nhờ retry ngẫu nhiên — nên chủ động.
4. Không dùng `bmgr` gì thêm — mọi path đều NPE.

## Bằng chứng

- Crash hôm nay (e2e install): `system_server_crash@1795000962959.txt` — NPE tại UserBackupManagerService.java:2113, triggered 18:22:42 (43s sau lệnh install 1795000919)
- Crash từ thí nghiệm bmgr: `system_server_crash@1795001586062.txt`
- logcat buffer giữ nguyên stack; guest uptime `13 min` tại lúc check, load 0.58 (đã hạ nhiệt sau restart)

---

## Phản biện & chẩn đoán sửa (đợt 2, cùng ngày — A/B trên VM live)

### Thí nghiệm đối chứng

| Thao tác | system_server | dropbox crash mới |
|---|---|---|
| `settings put global backup_enabled 0` | **crash → restart** | `crash@…375227` — NPE init-path `scheduleNextFullBackupJob` (line 2086) |
| `rm /data/backup/fb-schedule*` rồi `settings put backup_enabled 1` | **crash → restart** | `crash@…403631` — cùng stack init-path (line 2086) |
| `pm uninstall` + `adb install` PinyinIME (không đụng settings nào) | **pid 14254 giữ nguyên** | **0 file mới** |

### Chẩn đoán sửa

- `mFullBackupQueue` (ArrayList) null **trong mọi trường hợp** — kể cả khi `/data/backup/fb-schedule` tồn tại lẫn đã xóa → **không phải file journal corrupt**; là lỗi khởi tạo field trong build `UE1A.230829.036.A1` (google_apis API-34 r14).
- Hai đường trigger cùng chạm queue null:
  - **Init-path (line 2086):** `startServiceForUser → initializeBackupEnableState → setBackupEnabled → updateStateOnBackupEnabled → scheduleNextFullBackupJob` — bắn ra khi **settings `backup_enabled` bị ghi** hoặc BMS service (re)start. `settings put` gây crash **2/2 lần** = deterministic trigger.
  - **Dequeue-path (line 2113):** crash lịch sử trùng thời điểm install — xảy ra khi BMS đang chạy xử lý `PACKAGE_ADDED` và dequeue queue null. **Racy**, không deterministic: install sạch mới nhất không crash.
- Chẩn đoán cũ "mỗi lần install đều sập" bị confound: session chẩn đoán tự chạy `bmgr`/`settings` cùng cửa sổ với install.
- **`winapk` không ghi backup settings nào** → pipeline của ta không phải trigger. Nghị quyết vận hành: **cấm tuyệt đối `bmgr` + `settings put *backup*` trên image này**.

### Kết luận cho đường fix

- Không có host-side prevention: queue null ở code level, mọi cửa (settings, bmgr, fb-schedule delete) đều không cứu được.
- Mitigation #3 (đợi framework sau install) vẫn **đáng làm** như defense-in-depth cho crash racy — nhưng không ngăn được gì, chỉ tránh race.
- Root fix duy nhất: đổi sysimg revision hoặc custom AOSP (mục trên).
- `backup_enabled` file = `1`, queue hỏng vĩnh viễn → BMS dead-mỗi-khi-khởi-động; image cần thay.
