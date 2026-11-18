// winapk-display-host — native per-app window host (replaces Electron+SetParent hack).
//
// What upstream did wrong, done right here:
//  * Children are spawned CREATE_NO_WINDOW by the Rust side → no conhost
//    windows exist at all (upstream ran a 3-minute hide loop for them).
//  * We mark ourselves PerMonitorV2 BEFORE creating any window → no DPI
//    scaling mismatch between the host window and the emulator child.
//  * The emulator HWND is reparented once into our window and restyled to
//    WS_CHILD — we never AttachThreadInput-bounce; each window owns its
//    own message loop so a stall here can't freeze the emulator's input.
//  * Optional --crop l,t,r,b zooms the embedded display so only the app's
//    freeform bounds fill the client area (per-app window model).
//  * Optional --stream mode presents BGRA frames from a shared-memory
//    transport through D3D11 (see present_d3d11.cpp) — phase-2 gfxstream
//    consumer, with --test-pattern for standalone verification.
//
// Build: make -C native/winapk-display-host  (MinGW g++)

#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif

#include <windows.h>
#include <dwmapi.h>
#include <shellapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vector>

#include "present.h"

#pragma comment(lib, "dwmapi.lib")

static const wchar_t *CLS = L"WINapkDisplayHost";
static const int FIT_TIMER = 1;
static const int DEAD_TIMER = 2;

struct Opts {
    DWORD emuPid = 0;
    wchar_t title[256] = L"WINapk";
    wchar_t icon[MAX_PATH] = {0};
    int w = 540, h = 960;
    bool hasCrop = false;
    int cropL = 0, cropT = 0, cropR = 0, cropB = 0;
    bool stream = false;
    wchar_t shmName[128] = {0};
    bool testPattern = false;
};

static HICON loadIcon(const Opts &o) {
    if (o.icon[0]) {
        HICON ic = (HICON)LoadImageW(NULL, o.icon, IMAGE_ICON, 0, 0,
                                     LR_LOADFROMFILE | LR_DEFAULTSIZE);
        if (ic) return ic;
    }
    return LoadIconW(NULL, IDI_APPLICATION);
}

// ---- find the emulator's top-level window for a given pid ----
struct FindCtx { DWORD pid; HWND best; DWORD bestArea; };
static BOOL CALLBACK enumEmu(HWND h, LPARAM lp) {
    FindCtx *c = (FindCtx *)lp;
    DWORD pid = 0;
    GetWindowThreadProcessId(h, &pid);
    if (pid != c->pid || !IsWindowVisible(h)) return TRUE;
    RECT r; GetWindowRect(h, &r);
    DWORD area = (DWORD)((r.right - r.left) * (r.bottom - r.top));
    if (area > c->bestArea) { c->bestArea = area; c->best = h; }
    return TRUE;
}
static HWND findEmuWindow(DWORD pid, DWORD timeoutMs) {
    FindCtx c{pid, NULL, 0};
    DWORD t0 = GetTickCount();
    while (!c.best && GetTickCount() - t0 < timeoutMs) {
        EnumWindows(enumEmu, (LPARAM)&c);
        if (!c.best) Sleep(150);
    }
    return c.best;
}

struct Host {
    Opts o;
    HWND self = NULL;
    HWND emu = NULL;
    SIZE lcd = {0, 0};   // emulator LCD size, captured pre-reparent
    int titlebar = 0;
    Presenter *presenter = nullptr;
};

static void fitChild(Host *h) {
    if (!h->emu || !IsWindow(h->emu)) return;
    RECT rc; GetClientRect(h->self, &rc);
    int cw = rc.right - rc.left, ch = rc.bottom - rc.top - h->titlebar;
    if (cw <= 0 || ch <= 0) return;

    int x = 0, y = h->titlebar, w = cw, ht = ch;
    if (h->o.hasCrop && h->lcd.cx > 0) {
        // Zoom the child so the app's freeform rect fills the client.
        int crw = std::max(1, h->o.cropR - h->o.cropL);
        int crh = std::max(1, h->o.cropB - h->o.cropT);
        double s = std::min((double)cw / crw, (double)ch / crh);
        w = (int)(h->lcd.cx * s);
        ht = (int)(h->lcd.cy * s);
        x = (cw - crw * s) / 2 - (int)(h->o.cropL * s);
        y = h->titlebar + (ch - crh * s) / 2 - (int)(h->o.cropT * s);
    }
    SetWindowPos(h->emu, NULL, x, y, w, ht,
                 SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
}

static LRESULT CALLBACK wndProc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) {
    Host *h = (Host *)GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    switch (msg) {
    case WM_CREATE: {
        auto cs = (CREATESTRUCTW *)lp;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (LONG_PTR)cs->lpCreateParams);
        h = (Host *)cs->lpCreateParams;
        SetTimer(hwnd, FIT_TIMER, 100, NULL);
        SetTimer(hwnd, DEAD_TIMER, 1000, NULL);
        return 0;
    }
    case WM_TIMER:
        if (wp == FIT_TIMER && h) {
            if (h->emu && !IsWindow(h->emu)) h->emu = NULL;
            if (!h->emu) {
                h->emu = findEmuWindow(h->o.emuPid, 0);
                if (h->emu) {
                    RECT r; GetClientRect(h->emu, &r);
                    h->lcd.cx = r.right; h->lcd.cy = r.bottom;
                    SetParent(h->emu, hwnd);
                    LONG st = GetWindowLongW(h->emu, GWL_STYLE);
                    st &= ~(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU |
                            WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_POPUP);
                    st |= WS_CHILD | WS_VISIBLE;
                    SetWindowLongW(h->emu, GWL_STYLE, st);
                    LONG ex = GetWindowLongW(h->emu, GWL_EXSTYLE);
                    SetWindowLongW(h->emu, GWL_EXSTYLE, ex & ~WS_EX_APPWINDOW);
                }
            }
            fitChild(h);
        } else if (wp == DEAD_TIMER && h && h->o.emuPid) {
            HANDLE pr = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, h->o.emuPid);
            DWORD code = 0;
            // Only kill our window on PROOF of death — a transient
            // OpenProcess failure (ACL, pid reuse race) shouldn't make
            // the app window vanish under the user.
            BOOL dead = pr && GetExitCodeProcess(pr, &code) && code != STILL_ACTIVE;
            if (pr) CloseHandle(pr);
            if (dead) DestroyWindow(hwnd);
        }
        return 0;
    case WM_SIZE:
        if (h) fitChild(h);
        if (h && h->presenter) presenterResize(h->presenter, LOWORD(lp), HIWORD(lp));
        return 0;
    case WM_ERASEBKGND:
        return 1;
    case WM_PAINT: {
        PAINTSTRUCT ps; BeginPaint(hwnd, &ps); EndPaint(hwnd, &ps);
        return 0;
    }
    case WM_ACTIVATE:
        // Clicking our frame should focus the embedded emulator too.
        if (h && h->emu && LOWORD(wp) != WA_INACTIVE) {
            SetFocus(h->emu);
        }
        return 0;
    case WM_CLOSE:
        DestroyWindow(hwnd);
        return 0;
    case WM_DESTROY:
        if (h && h->emu && IsWindow(h->emu)) {
            // Hand the emulator window back to the desktop so a host crash
            // never orphans a WS_CHILD emulator surface.
            SetParent(h->emu, NULL);
        }
        KillTimer(hwnd, FIT_TIMER);
        KillTimer(hwnd, DEAD_TIMER);
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static void usage() {
    wprintf(L"winapk-display-host --pid <emuPid> [--title T] [--icon file.ico] "
            L"[--size WxH] [--crop l,t,r,b] [--stream name | --test-pattern]\n");
}

int WINAPI wWinMain(HINSTANCE inst, HINSTANCE, PWSTR, int show) {
    // DPI awareness first — before ANY window exists.
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    Opts o;
    LPWSTR cmd = GetCommandLineW();
    int argc = 0; LPWSTR *argv = CommandLineToArgvW(cmd, &argc);
    for (int i = 1; i < argc; i++) {
        auto next = [&](const wchar_t *d) -> LPWSTR { return i + 1 < argc ? argv[++i] : (LPWSTR)d; };
        if (!wcscmp(argv[i], L"--pid")) o.emuPid = (DWORD)_wtoi(next(L"0"));
        else if (!wcscmp(argv[i], L"--title")) wcsncpy_s(o.title, next(L""), 255);
        else if (!wcscmp(argv[i], L"--icon")) wcsncpy_s(o.icon, next(L""), MAX_PATH - 1);
        else if (!wcscmp(argv[i], L"--size")) swscanf(next(L""), L"%dx%d", &o.w, &o.h);
        else if (!wcscmp(argv[i], L"--crop")) {
            o.hasCrop = 4 == swscanf(next(L""), L"%d,%d,%d,%d",
                                     &o.cropL, &o.cropT, &o.cropR, &o.cropB);
        } else if (!wcscmp(argv[i], L"--stream")) {
            o.stream = true; wcsncpy_s(o.shmName, next(L""), 127);
        } else if (!wcscmp(argv[i], L"--test-pattern")) {
            o.stream = true; o.testPattern = true;
        }
    }
    if (!o.emuPid && !o.testPattern) { usage(); return 2; }

    Host h; h.o = o;
    HICON icon = loadIcon(o);

    WNDCLASSW wc = {};
    wc.lpfnWndProc = wndProc;
    wc.hInstance = inst;
    wc.lpszClassName = CLS;
    wc.hIcon = icon;
    wc.hCursor = LoadCursorW(NULL, IDC_ARROW);
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    RegisterClassW(&wc);

    DWORD style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
    RECT want{0, 0, o.w, o.h};
    AdjustWindowRectExForDpi(&want, style, FALSE, 0,
                             GetDpiForSystem());
    h.self = CreateWindowExW(0, CLS, o.title, style,
                             CW_USEDEFAULT, CW_USEDEFAULT,
                             want.right - want.left, want.bottom - want.top,
                             NULL, NULL, inst, &h);
    if (!h.self) return 3;

    if (o.stream) {
        h.presenter = presenterCreate(h.self,
                                      o.testPattern ? nullptr : o.shmName,
                                      o.testPattern);
    }

    ShowWindow(h.self, show);
    UpdateWindow(h.self);

    MSG m;
    while (GetMessageW(&m, NULL, 0, 0) > 0) {
        TranslateMessage(&m);
        DispatchMessageW(&m);
    }
    if (h.presenter) presenterDestroy(h.presenter);
    if (argv) LocalFree(argv);
    return (int)m.wParam;
}
