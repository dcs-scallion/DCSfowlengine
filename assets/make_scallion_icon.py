"""Compose Scallion squadron icon: CZ flag disc, brushed-aluminum ring, Su-27 silhouette."""
from __future__ import annotations

import math
import unicodedata
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

SILHOUETTE = Path(
    r"C:\Users\Robo\.cursor\projects\c-GitHub-DCSfowlengine\assets\image-ec052a76-9381-4a4c-9f3f-3dab0cc93c17.png"
)
CZ_FLAG = Path(r"C:\Users\Robo\.cursor\projects\c-GitHub-DCSfowlengine\assets\cz-flag.png")
OUT_DIR = Path(r"C:\Users\Robo\.cursor\projects\c-GitHub-DCSfowlengine\assets")
OUT_HI = OUT_DIR / "scallion-squadron-icon-hi.png"
OUT_150 = OUT_DIR / "scallion-squadron-icon.png"
OUT_REPO = Path(r"C:\GitHub\DCSfowlengine\assets\scallion-squadron-icon.png")
OUT_REPO_HI = Path(r"C:\GitHub\DCSfowlengine\assets\scallion-squadron-icon-hi.png")

SIZE = 900
TEXT_RED = (210, 28, 36, 255)
TEXT_BLACK = (12, 10, 8, 255)
SHADOW_BLACK = (0, 0, 0, 210)
SHADOW_WHITE = (255, 255, 255, 230)
# Brushed aircraft aluminum (cool grey, not chrome-white)
BAND_ALUM = (168, 172, 178, 255)
RIM_HI = (220, 224, 230, 255)
RIM_MID = (164, 168, 174, 255)
RIM_LO = (104, 108, 114, 255)
BRUSH_HI = (236, 240, 244, 255)
BRUSH_LO = (88, 92, 98, 255)
# Side stars stay warm gold accents on the aluminum band
STAR_LO = (112, 86, 40, 255)
STAR_MID = (186, 150, 78, 255)
STAR_HI = (228, 198, 128, 255)
STAR_BRIGHT = (236, 210, 145, 255)


def clean_silhouette(path: Path) -> Image.Image:
    im = Image.open(path).convert("RGBA")
    px = im.load()
    w, h = im.size
    for y in range(h):
        for x in range(w):
            r, g, b, _ = px[x, y]
            lum = (r + g + b) / 3.0
            if lum >= 205:
                px[x, y] = (0, 0, 0, 0)
            else:
                px[x, y] = (0, 0, 0, 255)
    bbox = im.getbbox()
    if bbox:
        im = im.crop(bbox)
    return im


def shade_silhouette_white(sil: Image.Image) -> Image.Image:
    """Near-black body; highlights ~50 % darker than previous pass."""
    alpha = sil.getchannel("A")
    glow = alpha.filter(ImageFilter.GaussianBlur(radius=max(2, sil.size[0] // 80)))
    rim = Image.new("RGBA", sil.size, (0, 0, 0, 0))
    rp = rim.load()
    ap = alpha.load()
    gp = glow.load()
    w, h = sil.size
    for y in range(h):
        for x in range(w):
            a = ap[x, y]
            if a < 8:
                continue
            edge = max(0, gp[x, y] - a)
            nx = (x / max(1, w - 1)) - 0.35
            ny = (y / max(1, h - 1)) - 0.30
            bias = max(0.0, 1.0 - (nx * nx + ny * ny) * 1.8)
            hi = int(min(255, 14 + 22 * bias + edge * 0.09))
            shade = int(3 + 16 * bias)
            rp[x, y] = (shade + hi // 3, shade + hi // 3, shade + hi // 2, a)

    base = Image.new("RGBA", sil.size, (0, 0, 0, 0))
    base.paste((2, 2, 3, 255), mask=alpha)

    eroded = alpha.filter(ImageFilter.MinFilter(3))
    band = Image.new("L", sil.size, 0)
    bp = band.load()
    ep = eroded.load()
    for y in range(h):
        for x in range(w):
            if ap[x, y] > 200 and ep[x, y] < 180 and y < h * 0.62:
                bp[x, y] = 35
    white = Image.new("RGBA", sil.size, (180, 185, 195, 255))
    white.putalpha(band)
    base.alpha_composite(rim)
    base.alpha_composite(white)
    return base


def recessed_disc_shade(side: int) -> Image.Image:
    """Soft inset bevel: dark rim + top-left well shadow, faint bottom-right lip."""
    shade = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shade)
    # Concentric falloff from the rim inward
    for i in range(10):
        t = i / 9.0
        inset = max(1, int(side * 0.0045 * (i + 1)))
        a = int(110 * (1.0 - t) ** 1.35)
        sd.ellipse(
            (inset, inset, side - 1 - inset, side - 1 - inset),
            outline=(0, 0, 0, a),
            width=max(2, side // 70),
        )
    # Directional well: stronger top-left (light from upper-left)
    well = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    wd = ImageDraw.Draw(well)
    wd.ellipse(
        (-side * 0.15, -side * 0.22, side * 0.72, side * 0.55),
        fill=(0, 0, 0, 85),
    )
    # Faint highlight on bottom-right lip of the recess
    hi = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    hd = ImageDraw.Draw(hi)
    hd.ellipse(
        (side * 0.35, side * 0.40, side * 1.12, side * 1.15),
        fill=(255, 255, 255, 40),
    )
    # Keep shade near the rim only (punch a soft hole in the centre)
    hole = Image.new("L", (side, side), 255)
    ImageDraw.Draw(hole).ellipse(
        (side * 0.14, side * 0.14, side * 0.86, side * 0.86),
        fill=0,
    )
    hole = hole.filter(ImageFilter.GaussianBlur(radius=max(2, side // 28)))
    for part in (well, hi):
        part.putalpha(
            Image.composite(part.getchannel("A"), Image.new("L", (side, side), 0), hole)
        )
        shade.alpha_composite(part)
    shade.putalpha(
        Image.composite(shade.getchannel("A"), Image.new("L", (side, side), 0), hole)
    )
    return shade.filter(ImageFilter.GaussianBlur(radius=max(1, side // 90)))


def draw_cz_flag_disc(base: Image.Image, cx: float, cy: float, r: float, flag_path: Path):
    """Circular cutout of the Czech tricolor flag (fills centre disc)."""
    flag = Image.open(flag_path).convert("RGBA")
    side = int(math.ceil(r * 2.0))
    # Cover: scale so flag fills the circle, then centre-crop to square
    scale = max(side / flag.size[0], side / flag.size[1])
    nw = max(1, int(flag.size[0] * scale))
    nh = max(1, int(flag.size[1] * scale))
    flag = flag.resize((nw, nh), Image.Resampling.LANCZOS)
    # Shift crop left → colour junction moves right (mild; 0.18 was too far).
    left = (nw - side) // 2 - int(side * 0.10)
    top = (nh - side) // 2
    left = max(0, min(left, nw - side))
    top = max(0, min(top, nh - side))
    flag = flag.crop((left, top, left + side, top + side))

    disc = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    disc.alpha_composite(flag)
    mask = Image.new("L", (side, side), 0)
    ImageDraw.Draw(mask).ellipse((0, 0, side - 1, side - 1), fill=255)
    disc.putalpha(mask)

    base.alpha_composite(disc, (int(cx - side / 2), int(cy - side / 2)))


def load_font(size: int, bold: bool = True) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    candidates = []
    if bold:
        candidates.extend(
            (
                r"C:\Windows\Fonts\impact.ttf",
                r"C:\Windows\Fonts\arialbd.ttf",
                r"C:\Windows\Fonts\segoeuib.ttf",
            )
        )
    candidates.extend(
        (
            r"C:\Windows\Fonts\arialbd.ttf",
            r"C:\Windows\Fonts\segoeuib.ttf",
            r"C:\Windows\Fonts\arial.ttf",
        )
    )
    for p in candidates:
        try:
            return ImageFont.truetype(p, size=size)
        except OSError:
            continue
    return ImageFont.load_default()


def clock_to_math(deg: float) -> float:
    return math.radians(deg - 90)


def star_polygon(
    cx: float, cy: float, r_out: float, r_in: float, rot_deg: float = -90.0
) -> list[tuple[float, float]]:
    pts: list[tuple[float, float]] = []
    for i in range(10):
        ang = math.radians(rot_deg + i * 36.0)
        r = r_out if i % 2 == 0 else r_in
        pts.append((cx + r * math.cos(ang), cy + r * math.sin(ang)))
    return pts


def draw_shaded_gold_star(
    base: Image.Image, cx: float, cy: float, r: float
) -> None:
    """Five-point star with light/dark gold shading."""
    r_in = r * 0.38
    layer = Image.new("RGBA", base.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    # Soft drop into the band
    shadow = [(x + r * 0.06, y + r * 0.08) for x, y in star_polygon(cx, cy, r, r_in)]
    d.polygon(shadow, fill=(60, 42, 18, 110))

    # Dark gold body
    body = star_polygon(cx, cy, r, r_in)
    d.polygon(body, fill=STAR_LO)

    # Mid gold inset
    mid = star_polygon(cx - r * 0.02, cy - r * 0.03, r * 0.92, r_in * 0.92)
    d.polygon(mid, fill=STAR_MID)

    # Bright highlight (upper-left bias)
    hi = star_polygon(cx - r * 0.06, cy - r * 0.08, r * 0.72, r_in * 0.72)
    d.polygon(hi, fill=(*STAR_BRIGHT[:3], 200))

    # Crisp rim
    d.polygon(body, outline=STAR_HI)

    blurred = layer.filter(ImageFilter.GaussianBlur(radius=max(1, int(r * 0.04))))
    base.alpha_composite(blurred)
    base.alpha_composite(layer)


def draw_side_stars(
    base: Image.Image,
    cx: float,
    cy: float,
    text_r: float,
    band_w: float,
) -> None:
    """Three shaded gold stars on each side of the ring (symmetric)."""
    star_r = band_w * 0.28
    # Clear side arcs between top/bottom text (clock degrees).
    right_deg = (78.0, 90.0, 102.0)
    left_deg = (258.0, 270.0, 282.0)
    for deg in (*right_deg, *left_deg):
        ang = clock_to_math(deg)
        sx = cx + text_r * math.cos(ang)
        sy = cy + text_r * math.sin(ang)
        draw_shaded_gold_star(base, sx, sy, star_r)


def draw_arc_text(
    base: Image.Image,
    text: str,
    cx: float,
    cy: float,
    radius: float,
    font: ImageFont.ImageFont,
    fill: tuple[int, int, int, int],
    center_deg: float,
    total_span_deg: float,
    invert: bool = False,
    shadow: tuple[int, int, int, int] | None = None,
    shadow_ox: float = 0,
    shadow_oy: float = 0,
    equal_spacing: bool = False,
    slot_scale: float = 2.44,
    fill_span: bool = False,
):
    n = len(text)
    if n == 0:
        return
    span = total_span_deg
    rot_extra = 180.0 if invert else 0.0

    if equal_spacing:
        # Monospace slots: every letter centered in an equal angular cell.
        widths_px = [max(1, font.getbbox(ch)[2] - font.getbbox(ch)[0]) for ch in text]
        w_deg = [w / max(1.0, radius) * (180.0 / math.pi) for w in widths_px]
        if n == 1:
            mids = [center_deg]
        else:
            slot = max(w_deg) * slot_scale
            total = slot * n
            if total > span or fill_span:
                slot = span / n
                total = span
            if invert:
                start = center_deg + total / 2
                mids = [start - slot * (i + 0.5) for i in range(n)]
            else:
                start = center_deg - total / 2
                mids = [start + slot * (i + 0.5) for i in range(n)]
    else:
        widths = []
        gap = max(2, int(getattr(font, "size", 20) * 0.05))
        for ch in text:
            b = font.getbbox(ch)
            widths.append(max(1, b[2] - b[0]))
        total_px = sum(widths) + gap * max(0, n - 1)
        if invert:
            cur = center_deg + span / 2
            direction = -1.0
        else:
            cur = center_deg - span / 2
            direction = 1.0
        mids = []
        for i in range(n):
            frac = widths[i] / total_px
            half = span * frac * 0.5
            mid = cur + direction * half
            mids.append(mid)
            step = span * frac
            if i < n - 1:
                step += span * (gap / total_px)
            cur += direction * step

    # Shared frame from base letters (NFD) so Ů/Ě feet match neighbours.
    def base_letter(ch: str) -> str:
        parts = unicodedata.normalize("NFD", ch)
        return parts[0] if parts else ch

    bodies = [font.getbbox(base_letter(ch)) for ch in text if not ch.isspace()]
    accent_tops = [font.getbbox(ch)[1] for ch in text if not ch.isspace()]
    frame_top = min(list(accent_tops) + [b[1] for b in bodies]) if bodies else 0
    frame_bot = max(b[3] for b in bodies) if bodies else 1
    frame_h = max(1, frame_bot - frame_top)

    def place(ch: str, mid: float, color: tuple[int, int, int, int], ox: float, oy: float):
        ang = clock_to_math(mid)
        b_base = font.getbbox(base_letter(ch))
        b_full = font.getbbox(ch)
        gw = max(1, b_full[2] - b_full[0], b_base[2] - b_base[0])
        glyph = Image.new("RGBA", (gw + 10, frame_h + 10), (0, 0, 0, 0))
        # Origin locks base-letter bottom to shared frame_bot; accents grow upward.
        origin_y = 5 + (frame_bot - frame_top) - b_base[3]
        ImageDraw.Draw(glyph).text((5 - b_base[0], origin_y), ch, font=font, fill=color)
        rot = mid + rot_extra
        glyph = glyph.rotate(-rot, expand=True, resample=Image.Resampling.BICUBIC)
        x = int(cx + radius * math.cos(ang) - glyph.size[0] / 2 + ox)
        y = int(cy + radius * math.sin(ang) - glyph.size[1] / 2 + oy)
        base.alpha_composite(glyph, (x, y))

    if shadow is not None:
        for ch, mid in zip(text, mids):
            place(ch, mid, shadow, shadow_ox, shadow_oy)
    for ch, mid in zip(text, mids):
        place(ch, mid, fill, 0, 0)


def donut(size: int, r_out: float, r_in: float, color: tuple[int, int, int, int]) -> Image.Image:
    cx = cy = size / 2
    mask = Image.new("L", (size, size), 0)
    md = ImageDraw.Draw(mask)
    md.ellipse((cx - r_out, cy - r_out, cx + r_out, cy + r_out), fill=255)
    md.ellipse((cx - r_in, cy - r_in, cx + r_in, cy + r_in), fill=0)
    layer = Image.new("RGBA", (size, size), color)
    layer.putalpha(mask)
    return layer


def plastic_rim(size: int, r_out: float, r_in: float) -> Image.Image:
    """Brushed-aluminum bezel: soft highlight / mid / shade."""
    w = r_out - r_in
    r1 = r_out
    r2 = r_out - w * 0.28
    r3 = r_out - w * 0.72
    r4 = r_in
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    layer.alpha_composite(donut(size, r1, r2, RIM_HI))
    layer.alpha_composite(donut(size, r2, r3, RIM_MID))
    layer.alpha_composite(donut(size, r3, r4, RIM_LO))
    blurred = layer.filter(ImageFilter.GaussianBlur(radius=max(1, int(w * 0.18))))
    mask = Image.new("L", (size, size), 0)
    md = ImageDraw.Draw(mask)
    cx = cy = size / 2
    md.ellipse((cx - r_out, cy - r_out, cx + r_out, cy + r_out), fill=255)
    md.ellipse((cx - r_in, cy - r_in, cx + r_in, cy + r_in), fill=0)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(blurred, (0, 0), mask)
    out.alpha_composite(donut(size, r_out, r_out - max(1.0, w * 0.12), (*BRUSH_HI[:3], 140)))
    return out


def brushed_alum_band(size: int, r_out: float, r_in: float) -> Image.Image:
    """Satin / brushed aluminum annulus with circumferential metal streaks."""
    cx = cy = size / 2
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    layer.alpha_composite(donut(size, r_out, r_in, BAND_ALUM))

    streaks = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    sd = ImageDraw.Draw(streaks)
    # Concentric hairlines ≈ brushed metal (light + faint dark for depth)
    for i in range(28):
        t = i / 27.0
        rr = r_in + (r_out - r_in) * t
        a_hi = 12 + int(16 * abs(math.sin(t * math.pi * 3)))
        a_lo = 6 + int(10 * abs(math.sin(t * math.pi * 3 + 0.7)))
        sd.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), outline=(*BRUSH_HI[:3], a_hi), width=1)
        if i % 3 == 0:
            sd.ellipse(
                (cx - rr - 0.5, cy - rr - 0.5, cx + rr + 0.5, cy + rr + 0.5),
                outline=(*BRUSH_LO[:3], a_lo),
                width=1,
            )

    band_mask = Image.new("L", (size, size), 0)
    bmd = ImageDraw.Draw(band_mask)
    bmd.ellipse((cx - r_out, cy - r_out, cx + r_out, cy + r_out), fill=255)
    bmd.ellipse((cx - r_in, cy - r_in, cx + r_in, cy + r_in), fill=0)

    streaks.putalpha(
        Image.composite(streaks.getchannel("A"), Image.new("L", (size, size), 0), band_mask)
    )
    layer.alpha_composite(streaks)
    return layer


def compose() -> Image.Image:
    s = SIZE
    cx = cy = s / 2
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))

    r_outer = s * 0.485
    gold_w = s * 0.015  # rim thickness (same both sides)
    band_w = s * 0.100 * 1.20  # +20 % outer ring width

    r_after_outer_rim = r_outer - gold_w
    r_after_band = r_after_outer_rim - band_w
    # Aluminum band meets the flag disc directly (no shaded inner bevel).
    r_disc = r_after_band

    draw_cz_flag_disc(img, cx, cy, r_disc, CZ_FLAG)

    sil = clean_silhouette(SILHOUETTE)
    max_w = int(r_disc * 1.72)
    max_h = int(r_disc * 1.08)
    scale = min(max_w / sil.size[0], max_h / sil.size[1])
    nw = max(1, int(sil.size[0] * scale))
    nh = max(1, int(sil.size[1] * scale))
    sil = sil.resize((nw, nh), Image.Resampling.LANCZOS)
    sil = shade_silhouette_white(sil)
    img.alpha_composite(sil, (int(cx - nw / 2), int(cy - nh / 2 - s * 0.005)))

    # Recessed well shade over flag + silhouette (rim only)
    side = int(math.ceil(r_disc * 2.0))
    shade = recessed_disc_shade(side)
    disc_mask = Image.new("L", (side, side), 0)
    ImageDraw.Draw(disc_mask).ellipse((0, 0, side - 1, side - 1), fill=255)
    shade.putalpha(
        Image.composite(shade.getchannel("A"), Image.new("L", (side, side), 0), disc_mask)
    )
    img.alpha_composite(shade, (int(cx - side / 2), int(cy - side / 2)))

    img.alpha_composite(brushed_alum_band(s, r_after_outer_rim, r_after_band))

    text_r = (r_after_outer_rim + r_after_band) / 2
    band_w = r_after_outer_rim - r_after_band
    draw_side_stars(img, cx, cy, text_r, band_w)

    font_top = load_font(int(s * 0.082), bold=True)
    font_bot = load_font(int(s * 0.048), bold=True)

    draw_arc_text(
        img,
        "SCALLION",
        cx,
        cy,
        text_r,
        font_top,
        TEXT_BLACK,
        center_deg=0,
        total_span_deg=138,
        invert=False,
        shadow=SHADOW_WHITE,
        shadow_ox=s * 0.0035,
        shadow_oy=s * 0.0045,
        equal_spacing=True,
    )
    draw_arc_text(
        img,
        "VĚRNI ZŮSTANEME",
        cx,
        cy,
        text_r,
        font_bot,
        TEXT_RED,
        center_deg=180,
        total_span_deg=120,
        invert=True,
        shadow=SHADOW_WHITE,
        shadow_ox=s * 0.0025,
        shadow_oy=s * 0.0030,
        equal_spacing=True,
        fill_span=True,
    )

    img.alpha_composite(plastic_rim(s, r_outer, r_after_outer_rim))

    clip = Image.new("L", (s, s), 0)
    ImageDraw.Draw(clip).ellipse((cx - r_outer, cy - r_outer, cx + r_outer, cy + r_outer), fill=255)
    out = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    out.paste(img, (0, 0), clip)
    return out


def save_150(img: Image.Image, path: Path) -> int:
    final = img.resize((150, 150), Image.Resampling.LANCZOS)
    a = final.getchannel("A")
    a = a.point(lambda v: 255 if v >= 128 else 0)
    final.putalpha(a)

    size = 10**9
    best: Image.Image | None = None
    for colors in (48, 40, 32, 28, 24, 20, 16, 12, 8):
        alpha = final.getchannel("A")
        rgb = final.convert("RGB").quantize(
            colors=colors, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE
        )
        rgba = rgb.convert("RGBA")
        rgba.putalpha(alpha)
        rgba.save(path, format="PNG", optimize=True, compress_level=9)
        size = path.stat().st_size
        print(f"colors={colors} -> {size} bytes")
        best = rgba
        if size <= 50 * 1024:
            break
    if best is not None and size > 50 * 1024:
        best.save(path, format="PNG", optimize=True, compress_level=9)
        size = path.stat().st_size
    return size


def main():
    badge = compose()
    badge.save(OUT_HI, format="PNG", optimize=True)
    print("hi-res", OUT_HI, OUT_HI.stat().st_size, f"{badge.size[0]}x{badge.size[1]}")

    size = save_150(badge, OUT_150)
    OUT_REPO.parent.mkdir(parents=True, exist_ok=True)
    OUT_REPO.write_bytes(OUT_150.read_bytes())
    OUT_REPO_HI.write_bytes(OUT_HI.read_bytes())
    print("saved", OUT_150, size)
    print("saved", OUT_REPO)
    print("saved", OUT_REPO_HI)


if __name__ == "__main__":
    main()
