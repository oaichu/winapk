# Boot Performance — E2E Measured (2026-11-18)

## Đo hôm nay (e2e `winapk install`, warm userdata)

- spawn emulator → Android ready: **265s** (~4.4 phút)
- Android ready → install + AOT queue xong: +50s → **tổng e2e 315s**
- Các lần boot lịch sử: 239s (warm nhất), 346–392s, 716s (first-boot + restorecon)
- qemu trong boot: ~2.2/8 core CPU, RAM 1.7GB, disk IO gần 0 ở pha CPU

## Phân rã 245.8s guest (kernel → enable_screen)

Nguồn: `boot_progress_*` (events log), `SystemServerTiming`, init log.

| # | Pha | Khoảng | Bound |
|---|-----|--------|-------|
| 1 | Kernel + init early | 0→21s | CPU |
| 2 | **late-fs: mount /data + FBE/vold** | 21→73s (**51.6s**) | **DISK** |
| 3 | post-fs-data → zygote (apex activation, dalvik-cache) | 73→117s (44s) | Disk+CPU |
| 4 | Zygote preload classes/resources | 117→147s (30.5s) | CPU |
| 5 | PackageManagerService scan | 147→163s (16s) | CPU |
| 6 | startBootstrapServices + PhaseActivityManagerReady | 163→205s (36.8s trong đó) | CPU |
| 7 | **SystemUserUnlock (giải mã FBE user 0)** | 205→246s (**33.8s**) | **DISK** |
| 8 | bootanim → done → winapk ready | 246→265s | CPU |

→ ~85–95s (35%) là các pha **disk-bound**; phần còn lại CPU-bound.

## Nguyên nhân xếp hạng

1. **`-no-snapshot` mỗi lần chạy** (config `snapshot=false` mặc định) → mỗi lần mở app sau khi VM tắt = 1 boot lạnh full 245s. Thiết kế chủ ý (tránh ram.bin GB + rot), nhưng là nhân tố lớn nhất về thời gian chờ.
2. **Home WINapk nằm trên USB HDD**: `E:` = HGST 500GB 5400rpm qua USB — đo tuần tự **41 MB/s**; `C:` SSD đo **1484 MB/s (36×)**. Mọi mount /data, FBE unlock, dalvik-cache, apex đều đập vào qcow2 trên HDD này.
   - Ràng buộc: **C: chỉ còn 5.6GB trống** → không thể dời nguyên khối home về SSD.
3. **WHPX thay vì AEHD**: `doctor` báo `gvm=false` — driver AEHD đã được provision ở `runtime\aehd\` (aehd.Inf + silent_install.bat) nhưng **chưa được cài**. AEHD nhanh hơn WHPX đáng kể ở các pha vCPU.
4. **CPU host cũ**: Haswell 4C/8T (~2014), không hybrid cores (pin P-core = no-op), GPU GTX 850M/HD 4600.
5. First-boot sau seed userdata tốn thêm ~450s (restorecon -R /data + FBE init) — one-time, đã tối ưu đúng.

## Khuyến nghị (theo tác động/công sức)

| Fix | Kết quả mong đợi | Cách |
|-----|------------------|------|
| **A. Bật snapshot**: `winapk config set snapshot true` | Boot lần 2+ trở xuống **~15–30s**; chi phí ram.bin ~2–3GB trên E: (còn 88GB) | 1 lệnh; sẵn sàng trong code (đã có nhánh config) |
| **B. Cài AEHD driver** (cần admin) | Mọi pha CPU nhanh hơn ~20–40% | `E:\WINapk\runtime\aehd\silent_install.bat` bằng terminal Admin |
| **C. Giảm LCD boot**: 720×1280/dpi 320 | Zygote/bootanim/SurfaceFlinger nhẹ hơn trên GPU cũ | `winapk config set` không có lcd — sửa config.toml trực tiếp (`lcd_width/lcd_height/lcd_dpi`) |
| **D. Tắt boot animation**: thêm prop `debug.sf.nobootanimation=1` vào `[engine] props` | Tiết kiệm ~6–10s + thấy sẵn sàng sớm hơn | config.toml `props` |
| **E. Dọn C:** ≥12–15GB rồi chuyển WINAPK_HOME về SSD | Boot nhanh hơn mạnh về lâu dài (mọi pha disk −70%+) | Dời app/data khác ra E:; `setx WINAPK_HOME C:\WINapk` + migrate thư mục |

A+B kết hợp kỳ vọng: boot lần 2+ **<30s**; boot lạnh còn ~150–180s.

## Bằng chứng thô

- Timeline CSV: `E:\WINapk\tmp\boot_timeline.csv` (poll 5s: state/boot/bootanim/logsize)
- e2e output: `E:\WINapk\tmp\e2e_install_out.log` (exit 0)
- winapk.log các mốc: spawn `1795000646` → ready `1795000911` → install done `1795000961`
- Disk: `winsat`-style đo tay PowerShell (50×4MB read): C: 1483.9 MB/s, E: 41 MB/s
- VM sau e2e vẫn đang chạy (pid 13852) — dùng tiếp được, hoặc `winapk stop`
