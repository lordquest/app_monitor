#!/usr/bin/env python3
"""把 assets/ 下各尺寸的图标像素打包成 Windows .ico。

输入是 `mkicon.ps1` 导出的 `icon-<size>.bgra`（顶行在前的 BGRA，尺寸由文件名给出）
与 `icon-<size>.png`。输出 `assets/app.ico`。

不用 Pillow：这台机器上没装，而且 ICO 容器本身很简单 ——
ICONDIR + 每张图的 ICONDIRENTRY + 图像数据。
64px 及以下用 32bpp DIB（兼容性最好，壳层与旧工具都认），
128/256 用 PNG（体积小一个数量级，Vista 以后都支持）。
"""

import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
PNG_FROM = 128  # 这个尺寸及以上用 PNG 编码，其余用 DIB


def dib_entry(size: int) -> bytes:
    """32bpp 图标位图：BITMAPINFOHEADER + bottom-up BGRA + AND 掩码。"""
    raw = (ASSETS / f"icon-{size}.bgra").read_bytes()
    assert len(raw) == size * size * 4, f"icon-{size}.bgra 尺寸不对"

    # 输入是顶行在前，DIB 要自下而上
    stride = size * 4
    flipped = b"".join(raw[y * stride:(y + 1) * stride] for y in range(size - 1, -1, -1))

    header = struct.pack(
        "<IiiHHIIiiII",
        40,            # biSize
        size,          # biWidth
        size * 2,      # biHeight：颜色数据 + 掩码，各 size 行
        1,             # biPlanes
        32,            # biBitCount
        0,             # biCompression = BI_RGB
        len(flipped),  # biSizeImage
        0, 0, 0, 0,
    )
    # AND 掩码：32bpp 下由 alpha 决定透明度，掩码全 0；每行按 4 字节对齐
    mask_row = ((size + 31) // 32) * 4
    mask = b"\x00" * (mask_row * size)
    return header + flipped + mask


def png_entry(size: int) -> bytes:
    return (ASSETS / f"icon-{size}.png").read_bytes()


def main() -> int:
    images = []
    for size in SIZES:
        data = png_entry(size) if size >= PNG_FROM else dib_entry(size)
        images.append((size, data))

    out = bytearray()
    out += struct.pack("<HHH", 0, 1, len(images))  # reserved, type=icon, count
    offset = 6 + 16 * len(images)
    for size, data in images:
        dim = 0 if size >= 256 else size  # 256 在目录项里记作 0
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in images:
        out += data

    target = ASSETS / "app.ico"
    target.write_bytes(bytes(out))
    print(f"{target}  {len(out)} bytes  ({len(images)} sizes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
