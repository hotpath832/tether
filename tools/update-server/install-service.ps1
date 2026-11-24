# Installs the Tether update server on a Windows server: copies the exe,
# registers a startup task that runs it as SYSTEM (restarted if it stops),
# opens the firewall port and starts it.
#
# Run in an elevated PowerShell next to tether-update-server.exe:
#   powershell -ExecutionPolicy Bypass -File .\install-service.ps1 -Port 8080
param(
    [string]$InstallDir = "C:\TetherUpdateServer",
    [int]$Port = 8080
)
$ErrorActionPreference = "Stop"

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script in an elevated (Administrator) PowerShell."
}

$exeSrc = Join-Path $PSScriptRoot "tether-update-server.exe"
if (-not (Test-Path $exeSrc)) { throw "tether-update-server.exe not found next to this script." }

$taskName = "TetherUpdateServer"
if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Get-Process tether-update-server -ErrorAction SilentlyContinue | Stop-Process -Force
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$dataDir = Join-Path $InstallDir "data"
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
Copy-Item $exeSrc (Join-Path $InstallDir "tether-update-server.exe") -Force

# Listen port (only written when there is no server.json yet, so a TLS setup
# you added by hand is kept on upgrades).
$cfg = Join-Path $dataDir "server.json"
if (-not (Test-Path $cfg)) {
    "{`n  `"listen`": `"0.0.0.0:$Port`",`n  `"tls_cert`": null,`n  `"tls_key`": null`n}`n" |
        Out-File -FilePath $cfg -Encoding ascii
}

$exe = Join-Path $InstallDir "tether-update-server.exe"
$log = Join-Path $InstallDir "server.log"
# cmd wrapper so the server's output goes to server.log.
$action = New-ScheduledTaskAction -Execute "cmd.exe" `
    -Argument "/c `"`"$exe`" --data `"$dataDir`" >> `"$log`" 2>&1`"" -WorkingDirectory $InstallDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -StartWhenAvailable
$sys = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings `
    -Principal $sys -Force | Out-Null

$ruleName = "Tether update server"
Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
$listen = (Get-Content $cfg -Raw | ConvertFrom-Json).listen
$listenPort = [int]($listen -split ":")[-1]
New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Protocol TCP -LocalPort $listenPort -Action Allow | Out-Null

Start-ScheduledTask -TaskName $taskName

# Wait (up to 20 s) until the server listens on its port.
$up = $false
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 500
    if (Get-NetTCPConnection -LocalPort $listenPort -State Listen -ErrorAction SilentlyContinue) {
        $up = $true
        break
    }
}

Write-Host ""
if (-not $up) {
    Write-Host "The server did not start listening on port $listenPort within 20 seconds." -ForegroundColor Red
    if (Test-Path $log) {
        Write-Host "Last lines of $log :"
        Get-Content $log -Tail 20
    }
    $info = Get-ScheduledTaskInfo -TaskName $taskName
    Write-Host "Task last result: $($info.LastTaskResult)"
    exit 1
}
Write-Host "Tether update server is running (task '$taskName', port $listenPort)." -ForegroundColor Green
Write-Host "Dashboard: http://<this server's address>:$listenPort/admin"
$pwFile = Join-Path $dataDir "INITIAL_ADMIN_PASSWORD.txt"
if (Test-Path $pwFile) {
    Write-Host "First admin password: $((Get-Content $pwFile)[0])  (change it in the dashboard)" -ForegroundColor Yellow
}
Write-Host "Log: $log"
