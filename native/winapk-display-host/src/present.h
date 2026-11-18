#pragma once
#include <windows.h>

// D3D11 frame presenter: consumes BGRA frames from a shared-memory
// transport (or a synthetic test pattern) and presents through DXGI.
// See present_d3d11.cpp. All functions are safe to call from the window
// thread; the transport runs on its own thread.

struct Presenter;

// shmName may be null when testPattern is true.
Presenter *presenterCreate(HWND hwnd, const wchar_t *shmName, bool testPattern);
void presenterResize(Presenter *p, UINT w, UINT h);
void presenterDestroy(Presenter *p);

// Shared-memory transport contract (phase-2 gfxstream/gRPC producer
// writes this; --test-pattern synthesizes it):
//
//   struct FrameHeader {
//     UINT32 magic;      // 'WAF1'
//     UINT32 width;
//     UINT32 height;
//     UINT32 stride;     // bytes per row
//     UINT64 seq;        // frame counter; producer increments on new frame
//     UINT8  pixels[];   // BGRA8, stride*height bytes
//   };
#define WINAPK_FRAME_MAGIC 0x31464157u // 'WAF1'
struct FrameHeader {
    UINT32 magic;
    UINT32 width;
    UINT32 height;
    UINT32 stride;
    UINT64 seq;
};
