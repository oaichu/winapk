// present_d3d11.cpp — DXGI/D3D11 presenter for streamed guest frames.
//
// The reparent path (main.cpp) is the production transport today; this
// file is the phase-2 consumer for a zero-copy gfxstream/gRPC producer.
// It is REAL code: a worker thread blocks on the frame sequence number,
// uploads BGRA to a dynamic texture, copies to the swapchain backbuffer
// and Presents. `--test-pattern` runs the same path with synthetic
// frames so the pipeline is verifiable end-to-end without a guest.

#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <thread>
#include <atomic>
#include <vector>

#include "present.h"

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "dxgi.lib")

struct Presenter {
    HWND hwnd;
    ID3D11Device *dev = nullptr;
    ID3D11DeviceContext *ctx = nullptr;
    IDXGISwapChain *swap = nullptr;
    ID3D11Texture2D *stage = nullptr;
    UINT w = 0, h = 0;
    HANDLE shm = nullptr;
    const FrameHeader *hdr = nullptr;
    bool testPattern = false;
    std::atomic<bool> quit{false};
    std::thread worker;
    // The D3D11 immediate context is single-threaded — worker Map/Copy
    // vs UI-thread ClearState/ResizeBuffers must serialize or it's UB.
    SRWLOCK ctxLock = SRWLOCK_INIT;
};

static bool initD3D(Presenter *p) {
    DXGI_SWAP_CHAIN_DESC sd{};
    sd.BufferCount = 2;
    sd.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = p->hwnd;
    sd.SampleDesc.Count = 1;
    sd.Windowed = TRUE;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;

    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    D3D_FEATURE_LEVEL fl;
    HRESULT hr = D3D11CreateDeviceAndSwapChain(
        nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags,
        nullptr, 0, D3D11_SDK_VERSION,
        &sd, &p->swap, &p->dev, &fl, &p->ctx);
    if (FAILED(hr)) {
        // WARP fallback — stream mode still works without a GPU driver.
        hr = D3D11CreateDeviceAndSwapChain(
            nullptr, D3D_DRIVER_TYPE_WARP, nullptr, flags,
            nullptr, 0, D3D11_SDK_VERSION,
            &sd, &p->swap, &p->dev, &fl, &p->ctx);
    }
    return SUCCEEDED(hr);
}

static bool ensureStage(Presenter *p, UINT w, UINT hgt) {
    if (p->stage && p->w == w && p->h == hgt) return true;
    if (p->stage) p->stage->Release();
    D3D11_TEXTURE2D_DESC td{};
    td.Width = w; td.Height = hgt;
    td.MipLevels = 1; td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DYNAMIC;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
    if (FAILED(p->dev->CreateTexture2D(&td, nullptr, &p->stage))) return false;
    p->w = w; p->h = hgt;
    return true;
}

static void presentFrame(Presenter *p, const UINT8 *pixels, UINT w, UINT hgt, UINT stride) {
    AcquireSRWLockExclusive(&p->ctxLock);
    if (!ensureStage(p, w, hgt)) { ReleaseSRWLockExclusive(&p->ctxLock); return; }
    D3D11_MAPPED_SUBRESOURCE m{};
    if (FAILED(p->ctx->Map(p->stage, 0, D3D11_MAP_WRITE_DISCARD, 0, &m))) {
        ReleaseSRWLockExclusive(&p->ctxLock); return;
    }
    for (UINT y = 0; y < hgt; y++) {
        memcpy((UINT8 *)m.pData + y * m.RowPitch, pixels + y * stride, w * 4);
    }
    p->ctx->Unmap(p->stage, 0);

    ID3D11Resource *bb = nullptr;
    if (SUCCEEDED(p->swap->GetBuffer(0, __uuidof(ID3D11Resource), (void **)&bb))) {
        p->ctx->CopyResource(bb, p->stage);
        bb->Release();
        p->swap->Present(1, 0);
    }
    ReleaseSRWLockExclusive(&p->ctxLock);
}

// Synthetic BGRA frame — animates so a frozen pipeline is obvious.
static void genTestPattern(UINT8 *px, UINT w, UINT hgt, UINT stride, UINT64 seq) {
    for (UINT y = 0; y < hgt; y++) {
        for (UINT x = 0; x < w; x++) {
            UINT8 *q = px + y * stride + x * 4;
            q[0] = (UINT8)((x + seq) & 0xff);          // B
            q[1] = (UINT8)((y + seq * 2) & 0xff);      // G
            q[2] = (UINT8)(((x ^ y) + seq * 3) & 0xff);// R
            q[3] = 255;
        }
    }
}

static void workerMain(Presenter *p) {
    const UINT TW = 540, TH = 960;
    std::vector<UINT8> test;
    UINT64 lastSeq = 0;
    while (!p->quit.load()) {
        if (p->testPattern) {
            UINT stride = TW * 4;
            test.resize(stride * TH);
            genTestPattern(test.data(), TW, TH, stride, ++lastSeq);
            presentFrame(p, test.data(), TW, TH, stride);
            Sleep(16);
            continue;
        }
        if (!p->hdr) { Sleep(50); continue; }
        UINT64 seq = p->hdr->seq;
        if (seq == lastSeq) { Sleep(4); continue; }
        lastSeq = seq;
        // Producer-written fields get a sanity clamp before indexing —
        // a corrupt header can't send memcpy out of the mapped view.
        UINT fw = p->hdr->width, fh = p->hdr->height, fs = p->hdr->stride;
        if (!fw || !fh || fw > 8192 || fh > 8192 || fs < fw * 4 ||
            (UINT64)fs * fh > (1ULL << 31)) continue;
        const UINT8 *px = (const UINT8 *)p->hdr + sizeof(FrameHeader);
        presentFrame(p, px, fw, fh, fs);
    }
}

Presenter *presenterCreate(HWND hwnd, const wchar_t *shmName, bool testPattern) {
    auto *p = new Presenter();
    p->hwnd = hwnd;
    p->testPattern = testPattern;
    if (!initD3D(p)) { delete p; return nullptr; }
    if (!testPattern && shmName && *shmName) {
        p->shm = OpenFileMappingW(FILE_MAP_READ, FALSE, shmName);
        if (p->shm) {
            p->hdr = (const FrameHeader *)MapViewOfFile(p->shm, FILE_MAP_READ, 0, 0, 0);
        }
    }
    p->worker = std::thread(workerMain, p);
    return p;
}

void presenterResize(Presenter *p, UINT w, UINT h) {
    if (!p || !p->swap || w == 0 || h == 0) return;
    AcquireSRWLockExclusive(&p->ctxLock);
    p->ctx->ClearState();
    p->swap->ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, 0);
    ReleaseSRWLockExclusive(&p->ctxLock);
}

void presenterDestroy(Presenter *p) {
    if (!p) return;
    p->quit = true;
    if (p->worker.joinable()) p->worker.join();
    if (p->hdr) UnmapViewOfFile(p->hdr);
    if (p->shm) CloseHandle(p->shm);
    if (p->stage) p->stage->Release();
    if (p->swap) p->swap->Release();
    if (p->ctx) p->ctx->Release();
    if (p->dev) p->dev->Release();
    delete p;
}
