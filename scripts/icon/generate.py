#!/usr/bin/env python3
"""Regenerate assets/icon/ from the logo (docs/decisions.md D52).

    python3 scripts/icon/generate.py            # reads assets/logo/VeloX.svg
    python3 scripts/icon/generate.py logo.png   # or any 8-bit RGBA PNG

Pure standard library on purpose: no Pillow/ImageMagick needed, so the
assets can be regenerated on any machine that has Python 3. The SVG is
expected to wrap a single base64 PNG (`<image href="data:image/png;base64,...">`);
that PNG is extracted, padded to a square, and area-averaged (alpha
premultiplied, to avoid dark fringes) down to every size the outputs need:

- velox.ico      256 (PNG entry) / 128 / 64 / 48 / 32 / 16 (BMP entries)
- velox-256.png  reserved for a future macOS .icns
- velox-128.png  the runtime window icon (include_bytes! in ui/window.rs)
"""
import base64, os, re, struct, sys, zlib

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def load_source(path):
    data = open(path, 'rb').read()
    if path.lower().endswith('.svg'):
        m = re.search(rb'base64,([^"]+)"', data)
        if not m:
            sys.exit(f'{path}: no embedded base64 PNG found')
        data = base64.b64decode(m.group(1))
    return data


def read_png(d):
    assert d[:8] == b'\x89PNG\r\n\x1a\n'
    pos = 8; idat = b''; w = h = None
    while pos < len(d):
        ln, typ = struct.unpack('>I4s', d[pos:pos+8]); body = d[pos+8:pos+8+ln]; pos += 12 + ln
        if typ == b'IHDR':
            w, h, bd, ct, _, _, il = struct.unpack('>IIBBBBB', body)
            assert bd == 8 and ct == 6 and il == 0, (bd, ct, il)
        elif typ == b'IDAT': idat += body
    raw = zlib.decompress(idat); stride = w * 4; out = bytearray(); prev = bytearray(stride); p = 0
    for _ in range(h):
        f = raw[p]; line = bytearray(raw[p+1:p+1+stride]); p += 1 + stride
        for i in range(stride):
            a = line[i-4] if i >= 4 else 0; b = prev[i]; c = prev[i-4] if i >= 4 else 0
            if f == 1: line[i] = (line[i] + a) & 255
            elif f == 2: line[i] = (line[i] + b) & 255
            elif f == 3: line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                pp = a + b - c; pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out += line; prev = line
    return w, h, out

def pad_square(w, h, px):
    s = max(w, h); out = bytearray(s * s * 4); ox = (s - w) // 2; oy = (s - h) // 2
    for y in range(h):
        out[((y+oy)*s+ox)*4:((y+oy)*s+ox+w)*4] = px[y*w*4:(y+1)*w*4]
    return s, out

def resize(s, px, n):
    # Area-average downscale with premultiplied alpha (avoids dark fringes).
    out = bytearray(n * n * 4)
    for y in range(n):
        y0 = y * s // n; y1 = max(y0 + 1, (y + 1) * s // n)
        for x in range(n):
            x0 = x * s // n; x1 = max(x0 + 1, (x + 1) * s // n)
            r = g = b = a = 0
            for yy in range(y0, y1):
                base = (yy * s + x0) * 4
                for xx in range(x1 - x0):
                    i = base + xx * 4; al = px[i+3]
                    r += px[i] * al; g += px[i+1] * al; b += px[i+2] * al; a += al
            cnt = (y1 - y0) * (x1 - x0); o = (y * n + x) * 4
            if a:
                out[o] = r // a; out[o+1] = g // a; out[o+2] = b // a
            out[o+3] = a // cnt
    return out

def write_png(n, px):
    raw = b''.join(b'\x00' + bytes(px[y*n*4:(y+1)*n*4]) for y in range(n))
    def chunk(t, b): return struct.pack('>I', len(b)) + t + b + struct.pack('>I', zlib.crc32(t + b) & 0xffffffff)
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', n, n, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))

def bmp_entry(n, px):
    # 32bpp BGRA DIB, bottom-up, plus 1bpp AND mask (all zero: alpha channel rules).
    hdr = struct.pack('<IiiHHIIiiII', 40, n, n * 2, 1, 32, 0, n * n * 4, 0, 0, 0, 0)
    body = bytearray()
    for y in range(n - 1, -1, -1):
        for x in range(n):
            i = (y * n + x) * 4; body += bytes((px[i+2], px[i+1], px[i], px[i+3]))
    mask = bytes(((n + 31) // 32 * 4) * n)
    return hdr + body + mask

src = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, 'assets', 'logo', 'VeloX.svg')
out_dir = os.path.join(ROOT, 'assets', 'icon'); os.makedirs(out_dir, exist_ok=True)
w, h, px = read_png(load_source(src)); s, sq = pad_square(w, h, px)
print('source', w, h, 'padded to', s)
sizes = [256, 128, 64, 48, 32, 16]; imgs = {n: resize(s, sq, n) for n in sizes}
open(f'{out_dir}/velox-128.png', 'wb').write(write_png(128, imgs[128]))
open(f'{out_dir}/velox-256.png', 'wb').write(write_png(256, imgs[256]))
entries = []
for n in sizes:
    data = write_png(n, imgs[n]) if n == 256 else bmp_entry(n, imgs[n])
    entries.append((n, data))
ico = struct.pack('<HHH', 0, 1, len(entries)); off = 6 + 16 * len(entries); dirs = b''; blobs = b''
for n, data in entries:
    dirs += struct.pack('<BBBBHHII', n % 256, n % 256, 0, 0, 1, 32, len(data), off + sum(len(d) for _, d in entries[:entries.index((n, data))]))
    blobs += data
open(f'{out_dir}/velox.ico', 'wb').write(ico + dirs + blobs)
print('done')
