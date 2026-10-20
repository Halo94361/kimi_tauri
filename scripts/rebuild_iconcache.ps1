[Console]::OutputEncoding=[Text.Encoding]::UTF8
# 重建资源管理器图标缓存：结束 explorer -> 删除图标缓存库 -> 重启 explorer
Stop-Process -Name explorer -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 1500
Remove-Item "$env:LOCALAPPDATA\Microsoft\Windows\Explorer\iconcache_*.db" -Force -ErrorAction SilentlyContinue
Remove-Item "$env:LOCALAPPDATA\IconCache.db" -Force -ErrorAction SilentlyContinue
Start-Process explorer
Write-Output "icon cache rebuilt, explorer restarted"
