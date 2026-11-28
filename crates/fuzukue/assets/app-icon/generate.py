#!/usr/bin/env python3
"""Regenerate the app icon assets from icon-small.svg and icon-large.svg.

Run `python3 -I crates/fuzukue/assets/app-icon/generate.py` from the repository
root. It needs only the Python standard library and the librsvg and cairo shared
libraries. Output sizes below 256 px come from icon-small.svg, larger ones from
icon-large.svg.
"""

import ctypes as C
import math
import re
import struct
import xml.etree.ElementTree as ET
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
ANDROID_RES = ROOT / "android/app/src/main/res"
SVG = "{http://www.w3.org/2000/svg}"
LARGE_FROM = 256
# macOS icons keep a transparent margin around the tile (in 1024ths of the canvas).
MAC_MARGIN = 81

cairo = C.CDLL("libcairo.so.2")
rsvg = C.CDLL("librsvg-2.so.2")
gobject = C.CDLL("libgobject-2.0.so.0")


class Rectangle(C.Structure):
    _fields_ = [(name, C.c_double) for name in ("x", "y", "width", "height")]


cairo.cairo_image_surface_create.restype = C.c_void_p
cairo.cairo_image_surface_create.argtypes = [C.c_int, C.c_int, C.c_int]
cairo.cairo_create.restype = C.c_void_p
cairo.cairo_create.argtypes = [C.c_void_p]
cairo.cairo_surface_flush.argtypes = [C.c_void_p]
cairo.cairo_image_surface_get_data.restype = C.c_void_p
cairo.cairo_image_surface_get_data.argtypes = [C.c_void_p]
cairo.cairo_image_surface_get_stride.argtypes = [C.c_void_p]
cairo.cairo_destroy.argtypes = [C.c_void_p]
cairo.cairo_surface_destroy.argtypes = [C.c_void_p]
rsvg.rsvg_handle_new_from_data.restype = C.c_void_p
rsvg.rsvg_handle_new_from_data.argtypes = [C.c_char_p, C.c_size_t, C.c_void_p]
rsvg.rsvg_handle_render_document.argtypes = [C.c_void_p, C.c_void_p, C.POINTER(Rectangle), C.c_void_p]
gobject.g_object_unref.argtypes = [C.c_void_p]


def source(size):
    return (HERE / ("icon-large.svg" if size >= LARGE_FROM else "icon-small.svg")).read_bytes()


def render(svg, size, margin=0):
    """Return straight-alpha RGBA rows of a size x size rendering."""
    handle = rsvg.rsvg_handle_new_from_data(svg, len(svg), None)
    assert handle, "librsvg could not parse the SVG"
    surface = cairo.cairo_image_surface_create(0, size, size)  # ARGB32
    context = cairo.cairo_create(surface)
    inset = size * margin / 1024
    viewport = Rectangle(inset, inset, size - 2 * inset, size - 2 * inset)
    assert rsvg.rsvg_handle_render_document(handle, context, C.byref(viewport), None)
    cairo.cairo_surface_flush(surface)
    stride = cairo.cairo_image_surface_get_stride(surface)
    data = C.string_at(cairo.cairo_image_surface_get_data(surface), stride * size)
    rows = []
    for y in range(size):
        row = bytearray(size * 4)
        for x in range(size):
            # Little-endian ARGB32 is premultiplied B, G, R, A in memory.
            b, g, r, a = data[y * stride + x * 4 : y * stride + x * 4 + 4]
            if a:
                r, g, b = (min(255, (v * 255 + a // 2) // a) for v in (r, g, b))
            row[x * 4 : x * 4 + 4] = bytes((r, g, b, a))
        rows.append(bytes(row))
    cairo.cairo_destroy(context)
    cairo.cairo_surface_destroy(surface)
    gobject.g_object_unref(handle)
    return rows


def png(rows):
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    header = struct.pack(">IIBBBBB", len(rows[0]) // 4, len(rows), 8, 6, 0, 0, 0)
    pixels = zlib.compress(b"".join(b"\0" + row for row in rows), 9)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", pixels) + chunk(b"IEND", b"")


def dib(rows):
    """A 32-bit icon bitmap: header, bottom-up BGRA, then the 1-bit mask."""
    size = len(rows)
    colors = bytearray()
    mask = bytearray()
    stride = (size + 31) // 32 * 4
    for row in reversed(rows):
        bits = 0
        for x in range(size):
            r, g, b, a = row[x * 4 : x * 4 + 4]
            colors += bytes((b, g, r, a))
            bits = (bits << 1) | (a == 0)
        mask += (bits << (stride * 8 - size)).to_bytes(stride, "big")
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(colors) + len(mask), 0, 0, 0, 0)
    return header + bytes(colors) + bytes(mask)


def ico(sizes):
    # Only the 256 px entry is PNG; older resource tools and shell code expect bitmaps.
    images = [(png if size == 256 else dib)(render(source(size), size)) for size in sizes]
    offset = 6 + 16 * len(sizes)
    directory = struct.pack("<HHH", 0, 1, len(sizes))
    for size, image in zip(sizes, images):
        directory += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(image), offset)
        offset += len(image)
    return directory + b"".join(images)


def number(value):
    return f"{value:.4f}".rstrip("0").rstrip(".")


def vector_group(transform, children, indent):
    """Nest one Android group per SVG transform function, outermost first."""
    functions = re.findall(r"(\w+)\(([^)]*)\)", transform)
    if not functions:
        return children(indent)
    name, arguments = functions[0]
    values = [float(v) for v in re.split(r"[\s,]+", arguments.strip())]
    if name == "translate":
        attributes = {"translateX": values[0], "translateY": values[1] if len(values) > 1 else 0}
    elif name == "scale":
        attributes = {"scaleX": values[0], "scaleY": values[-1]}
    elif name == "rotate" and len(values) == 1:
        attributes = {"rotation": values[0]}
    else:
        raise ValueError(f"unsupported transform: {name}")
    pad = " " * indent
    opening = " ".join(f'android:{key}="{number(value)}"' for key, value in attributes.items())
    rest = " ".join(f"{n}({a})" for n, a in functions[1:])
    return f"{pad}<group {opening}>\n{vector_group(rest, children, indent + 4)}{pad}</group>\n"


def vector_node(node, inherited, indent):
    style = {**inherited, **node.attrib}
    style.pop("transform", None)

    def body(depth):
        if node.tag == SVG + "g":
            return "".join(vector_node(child, style, depth) for child in node)
        assert node.tag == SVG + "path", f"unsupported element: {node.tag}"
        pad = " " * depth
        attributes = [f'android:pathData="{style["d"]}"']
        if style.get("fill", "#000000") != "none":
            attributes.append(f'android:fillColor="{style.get("fill", "#000000")}"')
        if "stroke" in style:
            attributes.append(f'android:strokeColor="{style["stroke"]}"')
            attributes.append(f'android:strokeWidth="{style["stroke-width"]}"')
            for svg_name, android_name in (("stroke-linecap", "strokeLineCap"), ("stroke-linejoin", "strokeLineJoin")):
                if svg_name in style:
                    attributes.append(f'android:{android_name}="{style[svg_name]}"')
        joined = f"\n{pad}    ".join(attributes)
        return f"{pad}<path\n{pad}    {joined} />\n"

    return vector_group(node.get("transform", ""), body, indent)


def android_foreground(scale):
    """The glyph without its tile. 1024 viewport units span the 72 dp visible area of 108 dp."""
    offset = number(768 - 512 * scale)
    root = ET.fromstring((HERE / "icon-small.svg").read_bytes())
    body = "".join(vector_node(node, {}, 8) for node in root if node.tag != SVG + "rect")
    return f"""<?xml version="1.0" encoding="utf-8"?>
<!-- Generated by crates/fuzukue/assets/app-icon/generate.py. -->
<vector xmlns:android="http://schemas.android.com/apk/res/android"
    android:width="108dp"
    android:height="108dp"
    android:viewportWidth="1536"
    android:viewportHeight="1536">
    <group android:translateX="{offset}" android:translateY="{offset}" android:scaleX="{scale}" android:scaleY="{scale}">
{body}    </group>
</vector>
"""


def safe_zone_scale():
    """How far to shrink the glyph to fit the 66 dp circle that every launcher mask shows."""
    svg = (HERE / "icon-small.svg").read_text(encoding="utf-8")
    rows = render(re.sub(r"<rect [^>]*/>", "", svg).encode(), 1024)
    radius = max(
        math.hypot(x + 0.5 - 512, y + 0.5 - 512)
        for y, row in enumerate(rows)
        for x in range(1024)
        if row[x * 4 + 3] > 16
    )
    limit = 1024 * 33 / 72
    scale = min(1.0, math.floor(limit / radius * 100) / 100)
    print(f"Android safe zone: glyph radius {radius:.1f} of {limit:.1f}, scale {scale}")
    return scale


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data.encode() if isinstance(data, str) else data)
    print(path.relative_to(ROOT))


def main():
    write(HERE / "windows/fuzukue.ico", ico([16, 20, 24, 32, 40, 48, 64, 96, 128, 256]))
    write(HERE / "linux/window-icon.png", png(render(source(128), 128)))
    for side in (16, 32, 128, 256, 512):
        for scale, suffix in ((1, ""), (2, "@2x")):
            pixels = side * scale
            write(
                HERE / f"macos/fuzukue.iconset/icon_{side}x{side}{suffix}.png",
                png(render(source(pixels), pixels, MAC_MARGIN)),
            )
    for density, pixels in (("mdpi", 48), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)):
        write(ANDROID_RES / f"mipmap-{density}/ic_launcher.png", png(render(source(pixels), pixels)))
    write(ANDROID_RES / "drawable/ic_launcher_foreground.xml", android_foreground(safe_zone_scale()))


if __name__ == "__main__":
    main()
