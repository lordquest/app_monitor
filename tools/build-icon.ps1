Add-Type -AssemblyName System.Drawing

# App Monitor 图标：蓝色渐变圆角方块 + 白色「下载到盘」符号。
# 全部按归一化坐标算，逐尺寸直接绘制（比先画大图再缩，小尺寸更干净）。
#
# 用法：powershell -File tools/build-icon.ps1  然后 python tools/pack_ico.py
# 产出 assets/app.ico（build.rs 会把它嵌进 exe）。

$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "assets"
New-Item -ItemType Directory -Force -Path $out | Out-Null

function Add-RoundRect($path, [double]$x, [double]$y, [double]$w, [double]$h, [double]$r) {
    if ($r -le 0) {
        $path.AddRectangle([System.Drawing.RectangleF]::new([single]$x, [single]$y, [single]$w, [single]$h))
        return
    }
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
}

function New-IconBitmap([int]$S) {
    $bmp = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    # 底色：圆角方块 + 对角渐变
    $pad = 0.0
    $bg = New-Object System.Drawing.Drawing2D.GraphicsPath
    Add-RoundRect $bg ($S * $pad) ($S * $pad) ($S * (1 - 2 * $pad)) ($S * (1 - 2 * $pad)) ($S * 0.225)
    $c1 = [System.Drawing.Color]::FromArgb(255, 76, 141, 255)
    $c2 = [System.Drawing.Color]::FromArgb(255, 26, 74, 214)
    $rect = New-Object System.Drawing.RectangleF(0, 0, $S, $S)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $c1, $c2, 55.0)
    $g.FillPath($brush, $bg)
    $brush.Dispose()
    $bg.Dispose()

    # 前景：箭头（竖干 + 三角）+ 托盘
    $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)

    # 小尺寸加粗一档，避免 16px 下箭头糊成一根线
    $w = if ($S -le 24) { 0.175 } else { 0.145 }   # 竖干宽度
    $shaftTop = 0.205
    $shaftBot = 0.545
    $shaft = New-Object System.Drawing.Drawing2D.GraphicsPath
    Add-RoundRect $shaft ($S * (0.5 - $w / 2)) ($S * $shaftTop) ($S * $w) ($S * ($shaftBot - $shaftTop)) ($S * 0.012)
    $g.FillPath($white, $shaft)
    $shaft.Dispose()

    # 三角：底边略宽于竖干，尖端收在 0.735
    $head = New-Object System.Drawing.Drawing2D.GraphicsPath
    $hl = 0.5 - 0.215
    $hr = 0.5 + 0.215
    $hy = 0.505
    $tip = 0.745
    $pts = [System.Drawing.PointF[]]@(
        [System.Drawing.PointF]::new([single]($S * $hl), [single]($S * $hy)),
        [System.Drawing.PointF]::new([single]($S * $hr), [single]($S * $hy)),
        [System.Drawing.PointF]::new([single]($S * 0.5), [single]($S * $tip)),
        [System.Drawing.PointF]::new([single]($S * $hl), [single]($S * $hy))
    )
    $head.AddPolygon($pts)
    $g.FillPath($white, $head)
    $head.Dispose()

    # 托盘：一条圆头横线
    $tray = New-Object System.Drawing.Drawing2D.GraphicsPath
    Add-RoundRect $tray ($S * 0.275) ($S * 0.825) ($S * 0.45) ($S * 0.075) ($S * 0.0375)
    $g.FillPath($white, $tray)
    $tray.Dispose()

    $white.Dispose()
    $g.Dispose()
    return $bmp
}

foreach ($S in 16, 20, 24, 32, 40, 48, 64, 128, 256) {
    $bmp = New-IconBitmap $S
    $bmp.Save("$out\icon-$S.png", [System.Drawing.Imaging.ImageFormat]::Png)

    # 同时导出一份 BGRA 原始像素（顶行在前），交给 build 前的打包脚本拼成 .ico，
    # 这样就不必在 Python 里再实现一遍 PNG 解码。
    $rect = [System.Drawing.Rectangle]::new(0, 0, $S, $S)
    $locked = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bytes = [byte[]]::new($S * $S * 4)
    [System.Runtime.InteropServices.Marshal]::Copy($locked.Scan0, $bytes, 0, $bytes.Length)
    $bmp.UnlockBits($locked)
    [System.IO.File]::WriteAllBytes("$out\icon-$S.bgra", $bytes)

    $bmp.Dispose()
}

# 大图给 README / 商店页用
$big = New-IconBitmap 512
$big.Save("$out\icon.png", [System.Drawing.Imaging.ImageFormat]::Png)
$big.Dispose()

Get-ChildItem $out | Select-Object Name, Length | Format-Table -AutoSize
