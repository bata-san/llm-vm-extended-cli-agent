Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$base = Split-Path -Parent $MyInvocation.MyCommand.Path
$target = Join-Path $base 'start-controller.cmd'
if (-not (Test-Path $target)) { throw "start-controller.cmd not found: $target" }

$startup = [Environment]::GetFolderPath('Startup')
$shortcutPath = Join-Path $startup 'ThinkServe Switch.lnk'
$ws = New-Object -ComObject WScript.Shell
$shortcut = $ws.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $target
$shortcut.WorkingDirectory = $base
$shortcut.Description = 'ThinkServe Switch tray controller'
$shortcut.Save()

Write-Host "Installed startup shortcut: $shortcutPath"
