# ThinkServe Switch
# MIT License - see LICENSE
# Windows tray controller for llama.cpp + Tailscale Serve.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$script:AppName = 'ThinkServe Switch'
$script:BaseDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$script:ConfigPath = Join-Path $script:BaseDir 'config.json'
$script:StatePath = Join-Path $script:BaseDir 'runtime.json'
$script:LogDir = Join-Path $script:BaseDir 'logs'
$script:Config = $null
$script:ServerProcess = $null
$script:CurrentMode = 'FREE_GPU'
$script:ChangingMode = $false
$script:ExitRequested = $false
$script:RestoreTimer = $null
$script:StatusTimer = $null

New-Item -ItemType Directory -Force -Path $script:LogDir | Out-Null

. (Join-Path $script:BaseDir 'lib\Common.ps1')
. (Join-Path $script:BaseDir 'lib\Server.ps1')
. (Join-Path $script:BaseDir 'lib\UI.ps1')

$script:Config = Load-Config
Initialize-Tray

# First run: make configuration visible immediately when defaults still point to placeholders.
if (-not (Resolve-Executable $script:Config.LlamaServerPath) -or -not (Test-Path $script:Config.ModelPath)) {
    Show-Settings
}

$state = Load-RuntimeState
if ($state -and $state.mode) { $script:CurrentMode = [string]$state.mode }
Update-TrayState

if ([bool]$script:Config.RestoreLastMode -and $state -and $state.mode) {
    # Defer mode restoration until the tray loop has started.
    $restoreTimer = New-Object System.Windows.Forms.Timer
    $restoreTimer.Interval = 500
    $restoreTimer.Add_Tick({
        $script:RestoreTimer.Stop()
        $mode = [string](Load-RuntimeState).mode
        if ($mode -in @('SERVER','LOCAL_PRIORITY','FREE_GPU')) { Set-Mode $mode }
    })
    $script:RestoreTimer = $restoreTimer
    $restoreTimer.Start()
}
else {
    $script:CurrentMode = 'FREE_GPU'
    Update-TrayState
}

[System.Windows.Forms.Application]::Run()

if ($script:StatusTimer) { $script:StatusTimer.Stop(); $script:StatusTimer.Dispose() }
if ($script:RestoreTimer) { $script:RestoreTimer.Dispose() }
if ($script:Tray) { $script:Tray.Dispose() }
