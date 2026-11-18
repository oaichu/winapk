#!/usr/bin/env python3
"""WINapk logo generator — procedural geometric mark.

Concept: bold rounded "W" that reads as a robot face.
  - Two thin antennae angling out of the outer top peaks (Android robot cue)
  - Two eyes punched as negative-space holes on the inner rising strokes
  - Analytic diagonal gradient Android-green -> Windows-blue (no banding)

Renders at 4x supersample then downsamples (LANCZOS) for clean AA.

Outputs (into packaging/assets/logo/):
  logo-mark.png        mark on white                    (the logo)
  logo-lockup.png      mark + "winapk" wordmark on white
  icon-tile.png        gradient squircle + white mark   (app icon source)
  app.ico              multi-size ICO (16..256)
  preview.png          contact sheet for vision review
"""
import math
import os
from PIL import Image, ImageDraw, ImageFont

SS = 4                      # supersample factor
OUT = os.path.join(os.path.dirname(__file__), "..", "..", "packaging", "assets", "logo")
OUT = os.path.abspath(OUT)

# palette
GREEN = (61, 220, 132)      # #3DDC84 Android green
TEAL  = (18, 184, 166)      # mid stop
BLUE  = (0, 120, 212)       # #0078D4 Windows blue
INK   = (23, 23, 23)

# ---------- mark geometry (unit space 0..1000) ----------
STROKE = 168.0
W_PATH = [(95, 245), (288, 775), (500, 330), (712, 775), (905, 245)]
EYE_R = 56.0
EYE_T = 0.66                # position along valley->center-peak segment
ANT_LEN = 195.0
ANT_W = 58.0
ANT_ANG = math.radians(34)  # tilt outward from vertical
ANT_GAP = 48.0              # visible air gap between antenna and peak (Android-style float)
MARK_DY = 22.0              # optical centering nudge (down)


def lerp(a, b, t):
    return (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)


def mark_mask(size):
    """L-mode mask of the W-robot mark in a `size` px box (0..1000 unit space)."""
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    s = size / 1000.0

    def U(p):
        return ((p[0]) * s, (p[1] + MARK_DY) * s)

    # W body: thick polyline with round joints/caps
    pts = [U(p) for p in W_PATH]
    d.line(pts, fill=255, width=int(STROKE * s), joint="curve")
    r = STROKE * s / 2
    for p in pts:  # round caps/joints
        d.ellipse([p[0] - r, p[1] - r, p[0] + r, p[1] + r], fill=255)

    # antennae: detached bars floating above the outer peaks (Android-style),
    # angled outward, rounded both ends
    for peak, sign in ((W_PATH[0], -1), (W_PATH[-1], 1)):
        dx = math.sin(ANT_ANG) * sign
        dy = -math.cos(ANT_ANG)
        a = (peak[0] + dx * (ANT_GAP + ANT_LEN * 0.0),
             peak[1] + dy * (ANT_GAP + ANT_LEN * 0.0))
        b = (peak[0] + dx * (ANT_GAP + ANT_LEN),
             peak[1] + dy * (ANT_GAP + ANT_LEN))
        a, b = U(a), U(b)
        d.line([a, b], fill=255, width=int(ANT_W * s))
        rr = ANT_W * s / 2
        for p in (a, b):
            d.ellipse([p[0] - rr, p[1] - rr, p[0] + rr, p[1] + rr], fill=255)

    # eyes: punch holes on the two inner rising strokes (valley -> centre peak)
    for seg in ((W_PATH[1], W_PATH[2]), (W_PATH[3], W_PATH[2])):
        c = lerp(seg[0], seg[1], EYE_T)
        c = U(c)
        rr = EYE_R * s
        d.ellipse([c[0] - rr, c[1] - rr, c[0] + rr, c[1] + rr], fill=0)
    return m


def diagonal_gradient(size, stops, angle=48):
    """Analytic multi-stop diagonal RGB gradient (projects onto the angle axis
    — no rotation, so no banding seams in corners). Computed at low res on the
    projection axis then upscaled; the field is smooth so this is lossless."""
    lo = 256
    th = math.radians(angle)
    ux, uy = math.cos(th), math.sin(th)
    corners = [0, size * ux, size * uy, size * (ux + uy)]
    pmin, pmax = min(corners), max(corners)
    span = pmax - pmin
    # projection map at low res (values 0..255 ~ t across the box)
    scale = lo / size
    tmap = Image.new("L", (lo, lo))
    tp = tmap.load()
    for y in range(lo):
        row_p = y / scale * uy
        for x in range(lo):
            t = ((x / scale * ux + row_p) - pmin) / span
            tp[x, y] = max(0, min(255, int(t * 255)))
    # colour LUT through stops
    lut = []
    for ch in range(3):
        chlut = [0] * 256
        for i in range(256):
            t = i / 255.0
            for j in range(len(stops) - 1):
                t0, c0 = stops[j]
                t1, c1 = stops[j + 1]
                if t0 <= t <= t1:
                    f = (t - t0) / (t1 - t0) if t1 > t0 else 0
                    chlut[i] = int(c0[ch] + (c1[ch] - c0[ch]) * f)
                    break
            else:
                chlut[i] = stops[-1][1][ch]
        lut.extend(chlut)
    grad = tmap.point(lut, "RGB")
    return grad.resize((size, size), Image.BICUBIC)


STOPS = [(0.0, GREEN), (0.52, TEAL), (1.0, BLUE)]


def squircle_mask(size, pad=0):
    """Superellipse app-tile mask (Fluent-ish n=4.2)."""
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    n = 4.2
    half = size / 2 - pad
    cx = cy = size / 2
    pts = []
    steps = 720
    for i in range(steps + 1):
        t = 2 * math.pi * i / steps
        ct, st = math.cos(t), math.sin(t)
        x = half * math.copysign(abs(ct) ** (2 / n), ct)
        y = half * math.copysign(abs(st) ** (2 / n), st)
        pts.append((cx + x, cy + y))
    d.polygon(pts, fill=255)
    return m


def render_mark_on_white(sz=1024, bg=(255, 255, 255)):
    big = sz * SS
    grad = diagonal_gradient(big, STOPS)
    mask = mark_mask(big)
    img = Image.new("RGB", (big, big), bg)
    img.paste(grad, (0, 0), mask)
    return img.resize((sz, sz), Image.LANCZOS)


def render_tile(sz=1024, mark_scale=0.58):
    big = sz * SS
    tile = squircle_mask(big)
    grad = diagonal_gradient(big, STOPS)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    img.paste(grad, (0, 0), tile)
    # white mark centred (slightly above geometric centre for optical balance)
    ms = int(big * mark_scale)
    mm = mark_mask(ms)
    off = (big - ms) // 2
    white = Image.new("RGBA", (ms, ms), (255, 255, 255, 255))
    img.paste(white, (off, off), mm)
    return img.resize((sz, sz), Image.LANCZOS)


def render_lockup(sz=512):
    """mark + 'winapk' wordmark, horizontal lockup sized to real text bbox."""
    big = sz * SS
    try:
        font = ImageFont.truetype("C:/Windows/Fonts/segoeuib.ttf", int(big * 0.42))
    except OSError:
        font = ImageFont.truetype("C:/Windows/Fonts/arialbd.ttf", int(big * 0.42))
    probe = Image.new("RGB", (10, 10))
    pd = ImageDraw.Draw(probe)
    text = "winapk"
    tb = pd.textbbox((0, 0), text, font=font)
    tw, th = tb[2] - tb[0], tb[3] - tb[1]

    gap = int(big * 0.16)
    pad = int(big * 0.05)
    W = big + gap + tw + pad * 2
    img = Image.new("RGB", (W, big), (255, 255, 255))
    img.paste(render_mark_on_white(big), (0, 0))
    d = ImageDraw.Draw(img)
    d.text((big + gap - tb[0], (big - th) // 2 - tb[1]), text, font=font, fill=INK)
    return img.resize((int(W / SS), sz), Image.LANCZOS)


def render_preview():
    """Contact sheet: mark on white / dark / blue, tile, size strip, lockup."""
    S = 280
    sheet = Image.new("RGB", (S * 4, S * 3), (240, 240, 240))

    def cell(x, y, bg, what, scale=0.78):
        tile = Image.new("RGB", (S, S), bg)
        inner = what.resize((int(S * scale), int(S * scale)), Image.LANCZOS)
        tile.paste(inner, ((S - inner.width) // 2, (S - inner.height) // 2),
                   inner if inner.mode == "RGBA" else None)
        sheet.paste(tile, (x * S, y * S))

    mark = render_mark_on_white(1024)
    icon = render_tile(1024)
    cell(0, 0, (255, 255, 255), mark)
    cell(1, 0, (17, 17, 17), mark)
    cell(2, 0, (0, 90, 158), mark)
    cell(3, 0, (255, 255, 255), icon)
    # size strip (tile) — real-world legibility
    for i, px in enumerate((128, 64, 32, 16)):
        small = icon.resize((px, px), Image.LANCZOS)
        bg = Image.new("RGB", (S, S), (255, 255, 255))
        bg.paste(small, ((S - px) // 2, (S - px) // 2), small)
        sheet.paste(bg, (i * S, S))
    # lockup row
    lock = render_lockup(360)
    lw = min(lock.width, S * 4 - 60)
    lh = int(lock.height * lw / lock.width)
    bg = Image.new("RGB", (S * 4, S), (255, 255, 255))
    bg.paste(lock.resize((lw, lh), Image.LANCZOS),
             ((S * 4 - lw) // 2, (S - lh) // 2))
    sheet.paste(bg, (0, S * 2))
    return sheet


def render_msix_assets(assets_dir):
    """MSIX store-asset PNGs — replaces the placeholder tiles in packaging/assets/."""
    grad_bg = diagonal_gradient(310 * SS, STOPS)
    for name, w, h, ms in (
        ("Square44x44Logo.png", 44, 44, 0.52),
        ("Square150x150Logo.png", 150, 150, 0.56),
        ("Square310x310Logo.png", 310, 310, 0.56),
        ("StoreLogo.png", 50, 50, 0.52),
        ("Wide310x150Logo.png", 310, 150, 0.62),
    ):
        big_w, big_h = w * SS, h * SS
        img = Image.new("RGB", (big_w, big_h))
        grad = diagonal_gradient(max(big_w, big_h), STOPS)
        img.paste(grad.resize((big_w, big_h)), (0, 0))
        msize = int(big_h * ms)
        mm = mark_mask(msize)
        white = Image.new("RGBA", (msize, msize), (255, 255, 255, 255))
        img.paste(white, ((big_w - msize) // 2, (big_h - msize) // 2), mm)
        img.resize((w, h), Image.LANCZOS).save(os.path.join(assets_dir, name))
        print("   msix:", name)


def main():
    os.makedirs(OUT, exist_ok=True)
    render_mark_on_white(1024).save(os.path.join(OUT, "logo-mark.png"))
    render_lockup(512).save(os.path.join(OUT, "logo-lockup.png"))
    icon = render_tile(1024)
    icon.save(os.path.join(OUT, "icon-tile.png"))
    icon.save(os.path.join(OUT, "app.ico"),
              sizes=[(16, 16), (24, 24), (32, 32), (48, 48),
                     (64, 64), (128, 128), (256, 256)])
    render_preview().save(os.path.join(OUT, "preview.png"))
    render_msix_assets(os.path.dirname(OUT))
    print("wrote:", OUT)
    for f in sorted(os.listdir(OUT)):
        print("  ", f)


if __name__ == "__main__":
    main()
