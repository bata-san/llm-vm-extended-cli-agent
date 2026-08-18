# Common configuration, state, logging, and GPU helpers.

function Get-DefaultConfig {
    $tailscale = 'C:\Program Files\Tailscale\tailscale.exe'
    if (-not (Test-Path $tailscale)) { $tailscale = 'tailscale.exe' }

    [pscustomobject]@{
        LlamaServerPath = 'C:\llama.cpp\llama-server.exe'
        ModelPath = 'D:\models\model.gguf'
        TailscalePath = $tailscale
        LocalPort = 8080
        TailscaleHttpsPort = 443
        ContextSize = 16384
        GpuLayers = 99
        IdleSleepSeconds = 0
        ReadyTimeoutSeconds = 180
        ExtraArgs = ''
        RestoreLastMode = $true
    }
}

function Save-Config {
    param([object]$Config)
    $Config | ConvertTo-Json -Depth 4 | Set-Content -Path $script:ConfigPath -Encoding UTF8
}

function Load-Config {
    if (-not (Test-Path $script:ConfigPath)) {
        $cfg = Get-DefaultConfig
        Save-Config $cfg
        return $cfg
    }

    try {
        $cfg = Get-Content -Path $script:ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
        $defaults = Get-DefaultConfig
        foreach ($p in $defaults.PSObject.Properties) {
            if (-not ($cfg.PSObject.Properties.Name -contains $p.Name)) {
                $cfg | Add-Member -NotePropertyName $p.Name -NotePropertyValue $p.Value
            }
        }
        return $cfg
    }
    catch {
        [System.Windows.Forms.MessageBox]::Show(
            "config.json の読み込みに失敗しました。`n$($_.Exception.Message)",
            $script:AppName,
            'OK',
            'Error'
        ) | Out-Null
        return Get-DefaultConfig
    }
}

function Save-RuntimeState {
    param(
        [string]$Mode,
        [object]$ServerPid = $null
    )

    $obj = [ordered]@{
        mode = $Mode
        serverPid = if ($null -ne $ServerPid) { [int]$ServerPid } else { $null }
        updatedAt = (Get-Date).ToString('o')
    }
    $obj | ConvertTo-Json | Set-Content -Path $script:StatePath -Encoding UTF8
}

function Load-RuntimeState {
    if (-not (Test-Path $script:StatePath)) { return $null }
    try {
        return Get-Content -Path $script:StatePath -Raw -Encoding UTF8 | ConvertFrom-Json
    }
    catch {
        return $null
    }
}

function Resolve-Executable {
    param([string]$PathOrName)
    if ([string]::IsNullOrWhiteSpace($PathOrName)) { return $null }
    if (Test-Path $PathOrName) { return (Resolve-Path $PathOrName).Path }
    try {
        return (Get-Command $PathOrName -ErrorAction Stop).Source
    }
    catch {
        return $null
    }
}

function Write-Log {
    param([string]$Message)
    $line = "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $Message"
    Add-Content -Path (Join-Path $script:LogDir 'controller.log') -Value $line -Encoding UTF8
}

function Get-GpuStatusText {
    try {
        $nvsmi = Resolve-Executable 'nvidia-smi.exe'
        if (-not $nvsmi) { return 'GPU: nvidia-smi unavailable' }
        $out = & $nvsmi --query-gpu=memory.used,memory.total,utilization.gpu,temperature.gpu --format=csv,noheader,nounits 2>$null | Select-Object -First 1
        if (-not $out) { return 'GPU: unavailable' }
        $parts = $out -split ',' | ForEach-Object { $_.Trim() }
        if ($parts.Count -lt 4) { return 'GPU: unavailable' }
        return "GPU: $($parts[0])/$($parts[1]) MiB | $($parts[2])% | $($parts[3])°C"
    }
    catch {
        return 'GPU: unavailable'
    }
}
