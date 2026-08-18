# Windows Forms tray and settings UI.

function Show-Balloon {
    param([string]$Title, [string]$Text)
    $script:Tray.BalloonTipTitle = $Title
    $script:Tray.BalloonTipText = $Text
    $script:Tray.ShowBalloonTip(2500)
}

function Update-TrayState {
    $serverUp = [bool](Get-ManagedServerProcess)
    $script:ModeLabel.Text = "Mode: $($script:CurrentMode)"
    $script:ServerItem.Checked = $script:CurrentMode -eq 'SERVER'
    $script:LocalItem.Checked = $script:CurrentMode -eq 'LOCAL_PRIORITY'
    $script:FreeItem.Checked = $script:CurrentMode -eq 'FREE_GPU'
    $script:GpuLabel.Text = Get-GpuStatusText

    $suffix = if ($serverUp) { 'llama: ON' } else { 'llama: OFF' }
    $tip = "ThinkServe | $($script:CurrentMode) | $suffix"
    if ($tip.Length -gt 63) { $tip = $tip.Substring(0, 63) }
    $script:Tray.Text = $tip
}

function Show-Settings {
    $form = New-Object System.Windows.Forms.Form
    $form.Text = "$script:AppName - Settings"
    $form.StartPosition = 'CenterScreen'
    $form.Size = New-Object System.Drawing.Size(760, 525)
    $form.FormBorderStyle = 'FixedDialog'
    $form.MaximizeBox = $false
    $form.MinimizeBox = $false

    $fields = @(
        @{ Key='LlamaServerPath'; Label='llama-server.exe'; Y=20; Browse='exe' },
        @{ Key='ModelPath'; Label='GGUF model'; Y=65; Browse='gguf' },
        @{ Key='TailscalePath'; Label='tailscale.exe'; Y=110; Browse='exe' },
        @{ Key='LocalPort'; Label='Local port'; Y=165 },
        @{ Key='TailscaleHttpsPort'; Label='Tailscale HTTPS port'; Y=205 },
        @{ Key='ContextSize'; Label='Context size'; Y=245 },
        @{ Key='GpuLayers'; Label='GPU layers'; Y=285 },
        @{ Key='IdleSleepSeconds'; Label='Idle sleep sec (0=off)'; Y=325 },
        @{ Key='ReadyTimeoutSeconds'; Label='Ready timeout sec'; Y=365 },
        @{ Key='ExtraArgs'; Label='Extra llama args'; Y=405 }
    )

    $controls = @{}
    foreach ($f in $fields) {
        $label = New-Object System.Windows.Forms.Label
        $label.Text = $f.Label
        $label.Location = New-Object System.Drawing.Point(15, ($f.Y + 4))
        $label.Size = New-Object System.Drawing.Size(170, 24)
        $form.Controls.Add($label)

        $text = New-Object System.Windows.Forms.TextBox
        $text.Text = [string]$script:Config.($f.Key)
        $text.Location = New-Object System.Drawing.Point(190, $f.Y)
        $text.Size = New-Object System.Drawing.Size(455, 24)
        $form.Controls.Add($text)
        $controls[$f.Key] = $text

        if ($f.ContainsKey('Browse')) {
            $button = New-Object System.Windows.Forms.Button
            $button.Text = '...'
            $button.Location = New-Object System.Drawing.Point(655, ($f.Y - 1))
            $button.Size = New-Object System.Drawing.Size(50, 26)
            $key = $f.Key
            $kind = $f.Browse
            $button.Add_Click({
                $dlg = New-Object System.Windows.Forms.OpenFileDialog
                if ($kind -eq 'gguf') { $dlg.Filter = 'GGUF model (*.gguf)|*.gguf|All files (*.*)|*.*' }
                else { $dlg.Filter = 'Executable (*.exe)|*.exe|All files (*.*)|*.*' }
                if ($dlg.ShowDialog() -eq 'OK') { $controls[$key].Text = $dlg.FileName }
                $dlg.Dispose()
            }.GetNewClosure())
            $form.Controls.Add($button)
        }
    }

    $restore = New-Object System.Windows.Forms.CheckBox
    $restore.Text = '起動時に前回のモードを復元'
    $restore.Checked = [bool]$script:Config.RestoreLastMode
    $restore.Location = New-Object System.Drawing.Point(190, 440)
    $restore.Size = New-Object System.Drawing.Size(280, 24)
    $form.Controls.Add($restore)

    $save = New-Object System.Windows.Forms.Button
    $save.Text = 'Save'
    $save.Location = New-Object System.Drawing.Point(530, 440)
    $save.Size = New-Object System.Drawing.Size(85, 28)
    $save.DialogResult = 'OK'
    $form.AcceptButton = $save
    $form.Controls.Add($save)

    $cancel = New-Object System.Windows.Forms.Button
    $cancel.Text = 'Cancel'
    $cancel.Location = New-Object System.Drawing.Point(620, 440)
    $cancel.Size = New-Object System.Drawing.Size(85, 28)
    $cancel.DialogResult = 'Cancel'
    $form.CancelButton = $cancel
    $form.Controls.Add($cancel)

    if ($form.ShowDialog() -eq 'OK') {
        try {
            $newCfg = [pscustomobject]@{
                LlamaServerPath = $controls['LlamaServerPath'].Text.Trim()
                ModelPath = $controls['ModelPath'].Text.Trim()
                TailscalePath = $controls['TailscalePath'].Text.Trim()
                LocalPort = [int]$controls['LocalPort'].Text
                TailscaleHttpsPort = [int]$controls['TailscaleHttpsPort'].Text
                ContextSize = [int]$controls['ContextSize'].Text
                GpuLayers = [int]$controls['GpuLayers'].Text
                IdleSleepSeconds = [int]$controls['IdleSleepSeconds'].Text
                ReadyTimeoutSeconds = [int]$controls['ReadyTimeoutSeconds'].Text
                ExtraArgs = $controls['ExtraArgs'].Text.Trim()
                RestoreLastMode = [bool]$restore.Checked
            }
            Save-Config $newCfg
            $script:Config = $newCfg
            Show-Balloon $script:AppName '設定を保存しました。サーバー起動オプションの変更は次回ロード時に反映されます。'
        }
        catch {
            [System.Windows.Forms.MessageBox]::Show("設定値を確認してください。`n$($_.Exception.Message)", $script:AppName, 'OK', 'Error') | Out-Null
        }
    }
    $form.Dispose()
}

function Open-LocalUi {
    Start-Process "http://127.0.0.1:$($script:Config.LocalPort)/"
}

function Open-Logs {
    Start-Process explorer.exe $script:LogDir
}

function Initialize-Tray {
    $script:Tray = New-Object System.Windows.Forms.NotifyIcon
    $script:Tray.Icon = [System.Drawing.SystemIcons]::Application
    $script:Tray.Visible = $true
    $script:Tray.Text = $script:AppName

    $menu = New-Object System.Windows.Forms.ContextMenuStrip

    $script:ModeLabel = New-Object System.Windows.Forms.ToolStripMenuItem
    $script:ModeLabel.Enabled = $false
    [void]$menu.Items.Add($script:ModeLabel)

    $script:GpuLabel = New-Object System.Windows.Forms.ToolStripMenuItem
    $script:GpuLabel.Enabled = $false
    [void]$menu.Items.Add($script:GpuLabel)
    [void]$menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))

    $script:ServerItem = New-Object System.Windows.Forms.ToolStripMenuItem
    $script:ServerItem.Text = 'SERVER - Remote inference ON'
    $script:ServerItem.CheckOnClick = $false
    $script:ServerItem.Add_Click({ Set-Mode 'SERVER' })
    [void]$menu.Items.Add($script:ServerItem)

    $script:LocalItem = New-Object System.Windows.Forms.ToolStripMenuItem
    $script:LocalItem.Text = 'LOCAL PRIORITY - Remote OFF, model stays'
    $script:LocalItem.CheckOnClick = $false
    $script:LocalItem.Add_Click({ Set-Mode 'LOCAL_PRIORITY' })
    [void]$menu.Items.Add($script:LocalItem)

    $script:FreeItem = New-Object System.Windows.Forms.ToolStripMenuItem
    $script:FreeItem.Text = 'FREE GPU - Stop model / release VRAM'
    $script:FreeItem.CheckOnClick = $false
    $script:FreeItem.Add_Click({ Set-Mode 'FREE_GPU' })
    [void]$menu.Items.Add($script:FreeItem)

    [void]$menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))

    $openUi = New-Object System.Windows.Forms.ToolStripMenuItem
    $openUi.Text = 'Open local llama.cpp UI'
    $openUi.Add_Click({ Open-LocalUi })
    [void]$menu.Items.Add($openUi)

    $settings = New-Object System.Windows.Forms.ToolStripMenuItem
    $settings.Text = 'Settings...'
    $settings.Add_Click({ Show-Settings })
    [void]$menu.Items.Add($settings)

    $logs = New-Object System.Windows.Forms.ToolStripMenuItem
    $logs.Text = 'Open logs'
    $logs.Add_Click({ Open-Logs })
    [void]$menu.Items.Add($logs)

    [void]$menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))

    $exit = New-Object System.Windows.Forms.ToolStripMenuItem
    $exit.Text = 'Exit controller (server keeps current state)'
    $exit.Add_Click({
        $script:ExitRequested = $true
        $script:Tray.Visible = $false
        [System.Windows.Forms.Application]::ExitThread()
    })
    [void]$menu.Items.Add($exit)

    $script:Tray.ContextMenuStrip = $menu
    $script:Tray.Add_DoubleClick({ Open-LocalUi })

    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 5000
    $timer.Add_Tick({ Update-TrayState })
    $timer.Start()
    $script:StatusTimer = $timer
}
