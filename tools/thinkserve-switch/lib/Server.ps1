# llama.cpp and Tailscale process/mode control.

function Get-ManagedServerProcess {
    if ($script:ServerProcess -and -not $script:ServerProcess.HasExited) {
        return $script:ServerProcess
    }

    $state = Load-RuntimeState
    if (-not $state -or -not $state.serverPid) { return $null }

    try {
        $p = [System.Diagnostics.Process]::GetProcessById([int]$state.serverPid)
        if ($p.HasExited) { return $null }

        # Best-effort sanity check. If CIM access is unavailable, keep the PID check only.
        try {
            $cim = Get-CimInstance Win32_Process -Filter "ProcessId = $($p.Id)" -ErrorAction Stop
            if ($cim -and $cim.Name -notmatch '^llama-server(\.exe)?$') {
                return $null
            }
        }
        catch { }

        $script:ServerProcess = $p
        return $p
    }
    catch {
        return $null
    }
}

function Test-ServerReady {
    param([int]$Port)
    try {
        $r = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 2
        return $r.StatusCode -eq 200
    }
    catch {
        return $false
    }
}

function Start-LlamaServer {
    $existing = Get-ManagedServerProcess
    if ($existing) { return $existing }

    $serverExe = Resolve-Executable $script:Config.LlamaServerPath
    if (-not $serverExe) { throw "llama-server が見つかりません: $($script:Config.LlamaServerPath)" }
    if (-not (Test-Path $script:Config.ModelPath)) { throw "GGUFモデルが見つかりません: $($script:Config.ModelPath)" }

    $stdout = Join-Path $script:LogDir 'llama-server.stdout.log'
    $stderr = Join-Path $script:LogDir 'llama-server.stderr.log'

    $args = @(
        '-m', ('"' + $script:Config.ModelPath + '"'),
        '--host', '127.0.0.1',
        '--port', [string]$script:Config.LocalPort,
        '-c', [string]$script:Config.ContextSize,
        '--n-gpu-layers', [string]$script:Config.GpuLayers
    )

    if ([int]$script:Config.IdleSleepSeconds -gt 0) {
        $args += @('--sleep-idle-seconds', [string]$script:Config.IdleSleepSeconds)
    }

    if (-not ([string]::IsNullOrWhiteSpace([string]$script:Config.ExtraArgs))) {
        $args += [string]$script:Config.ExtraArgs
    }

    $p = Start-Process -FilePath $serverExe `
        -ArgumentList $args `
        -WorkingDirectory (Split-Path -Parent $serverExe) `
        -WindowStyle Hidden `
        -RedirectStandardOutput $stdout `
        -RedirectStandardError $stderr `
        -PassThru

    $script:ServerProcess = $p
    Save-RuntimeState -Mode $script:CurrentMode -ServerPid $p.Id
    return $p
}

function Wait-LlamaReady {
    $deadline = (Get-Date).AddSeconds([int]$script:Config.ReadyTimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        [System.Windows.Forms.Application]::DoEvents()
        $p = Get-ManagedServerProcess
        if (-not $p) {
            throw "llama-server が起動直後に終了しました。logs フォルダを確認してください。"
        }
        if (Test-ServerReady -Port ([int]$script:Config.LocalPort)) { return }
        Start-Sleep -Milliseconds 500
    }
    throw "llama-server の起動待ちがタイムアウトしました。"
}

function Stop-LlamaServer {
    $p = Get-ManagedServerProcess
    if (-not $p) {
        $script:ServerProcess = $null
        return
    }

    try {
        Stop-Process -Id $p.Id -Force -ErrorAction Stop
        try { $p.WaitForExit(10000) | Out-Null } catch { }
    }
    finally {
        $script:ServerProcess = $null
    }
}

function Invoke-Tailscale {
    param([string[]]$Arguments)

    $exe = Resolve-Executable $script:Config.TailscalePath
    if (-not $exe) { throw "Tailscale CLI が見つかりません: $($script:Config.TailscalePath)" }

    $output = & $exe @Arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw "Tailscale コマンドに失敗しました (exit $code):`n$output"
    }
    return $output.Trim()
}

function Enable-TailscaleServe {
    $httpsPort = [int]$script:Config.TailscaleHttpsPort
    $localPort = [int]$script:Config.LocalPort
    Invoke-Tailscale @('serve', '--bg', "--https=$httpsPort", "127.0.0.1:$localPort") | Out-Null
}

function Disable-TailscaleServe {
    try {
        $httpsPort = [int]$script:Config.TailscaleHttpsPort
        # Tailscale documents that the target may be omitted when disabling Serve.
        Invoke-Tailscale @('serve', "--https=$httpsPort", 'off') | Out-Null
    }
    catch {
        # If Serve is already off, do not block a local-priority/free-GPU transition.
        Write-Log "Disable Tailscale Serve warning: $($_.Exception.Message)"
    }
}

function Set-Mode {
    param([ValidateSet('SERVER','LOCAL_PRIORITY','FREE_GPU')][string]$Mode)
    if ($script:ChangingMode) { return }
    $script:ChangingMode = $true
    try {
        $script:ModeLabel.Text = "Switching → $Mode ..."
        [System.Windows.Forms.Application]::DoEvents()
        Write-Log "Mode change requested: $($script:CurrentMode) -> $Mode"

        switch ($Mode) {
            'SERVER' {
                $p = Start-LlamaServer
                Wait-LlamaReady
                Enable-TailscaleServe
                $script:CurrentMode = 'SERVER'
                Save-RuntimeState -Mode $script:CurrentMode -ServerPid $p.Id
                Show-Balloon $script:AppName 'SERVER: リモート推論を受け付けます。'
            }
            'LOCAL_PRIORITY' {
                Disable-TailscaleServe
                $p = Start-LlamaServer
                Wait-LlamaReady
                $script:CurrentMode = 'LOCAL_PRIORITY'
                Save-RuntimeState -Mode $script:CurrentMode -ServerPid $p.Id
                Show-Balloon $script:AppName 'LOCAL PRIORITY: 外部受付を止め、モデルは保持しています。'
            }
            'FREE_GPU' {
                Disable-TailscaleServe
                Stop-LlamaServer
                $script:CurrentMode = 'FREE_GPU'
                Save-RuntimeState -Mode $script:CurrentMode
                Show-Balloon $script:AppName 'FREE GPU: 推論サーバーを停止し、VRAMを解放しました。'
            }
        }
    }
    catch {
        Write-Log "Mode change error: $($_.Exception.Message)"
        [System.Windows.Forms.MessageBox]::Show(
            $_.Exception.Message,
            $script:AppName,
            'OK',
            'Error'
        ) | Out-Null
    }
    finally {
        $script:ChangingMode = $false
        Update-TrayState
    }
}
