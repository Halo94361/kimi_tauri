[Console]::OutputEncoding=[Text.Encoding]::UTF8
Add-Type -AssemblyName System.Drawing
$ico = [System.Drawing.Icon]::ExtractAssociatedIcon((Join-Path $PSScriptRoot "..\src-tauri\target\release\tauri_kimi.exe"))
$bmp = $ico.ToBitmap()
$bg = New-Object System.Drawing.Bitmap 256,256
$g = [System.Drawing.Graphics]::FromImage($bg)
$g.Clear([System.Drawing.Color]::Magenta)
$g.InterpolationMode = 'NearestNeighbor'
$g.DrawImage($bmp, 0, 0, 256, 256)
$bg.Save((Join-Path $PSScriptRoot "exe_icon_check.png"))
Write-Output "saved"
