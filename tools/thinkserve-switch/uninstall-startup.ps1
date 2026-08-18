Set-StrictMode -Version Latest
$startup = [Environment]::GetFolderPath('Startup')
$shortcutPath = Join-Path $startup 'ThinkServe Switch.lnk'
if (Test-Path $shortcutPath) {
    Remove-Item -Force $shortcutPath
    Write-Host "Removed: $shortcutPath"
} else {
    Write-Host 'Startup shortcut was not installed.'
}
