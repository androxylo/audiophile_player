<#
.SYNOPSIS
    Launches Audiophile Player (if not already running) and opens its local HTTP control UI.

.DESCRIPTION
    - If audiophile-player.exe is already running, detects its active localhost port (8085..8105)
      and opens the Web Control UI directly in the default browser without disturbing playback.
    - Otherwise, starts target\release\audiophile-player.exe in a clean console window (close or
      press Ctrl+C to stop and release the DAC), waits for the HTTP UI to come up, and opens it.
#>

$ErrorActionPreference = 'SilentlyContinue'

$repoRoot = Split-Path -Parent $PSScriptRoot
$exePath  = Join-Path $repoRoot 'target\release\audiophile-player.exe'

function Find-PlayerUiUrl {
    for ($port = 8085; $port -le 8105; $port++) {
        $tcp = New-Object System.Net.Sockets.TcpClient
        try {
            $iar = $tcp.BeginConnect('127.0.0.1', $port, $null, $null)
            if (-not $iar.AsyncWaitHandle.WaitOne(80, $false)) {
                continue
            }
            $tcp.EndConnect($iar)
        } catch {
            continue
        } finally {
            $tcp.Dispose()
        }

        try {
            $req = [System.Net.HttpWebRequest]::Create("http://127.0.0.1:$port/api/status")
            $req.Timeout = 300
            $req.ReadWriteTimeout = 300
            $resp = $req.GetResponse()
            try {
                $reader = New-Object System.IO.StreamReader($resp.GetResponseStream())
                $body = $reader.ReadToEnd()
                $reader.Dispose()
                if ($body -match '"dac"\s*:' -and $body -match '"mode"\s*:') {
                    return "http://127.0.0.1:$port"
                }
            } finally {
                $resp.Close()
            }
        } catch {
            continue
        }
    }
    return $null
}

if (-not (Test-Path -LiteralPath $exePath)) {
    Add-Type -AssemblyName PresentationFramework
    [System.Windows.MessageBox]::Show(
        "audiophile-player.exe was not found at:`n$exePath`n`nBuild it first with:`ncargo build --release --bin audiophile-player",
        "Audiophile Player",
        [System.Windows.MessageBoxButton]::OK,
        [System.Windows.MessageBoxImage]::Warning
    ) | Out-Null
    exit 1
}

# 1. If already running, just open the active Web Control UI in the browser.
$running = Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue
if ($running) {
    $url = Find-PlayerUiUrl
    if ($url) {
        Start-Process $url
        exit 0
    }
}

# 2. Launch audiophile-player.exe in a normal console window so status is visible and Ctrl+C/close stops cleanly.
Start-Process -FilePath $exePath -WorkingDirectory $repoRoot

# 3. Wait up to 6 seconds for the HTTP control server to bind, then open it in the default browser.
$deadline = (Get-Date).AddSeconds(6)
$uiUrl = $null
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    $uiUrl = Find-PlayerUiUrl
    if ($uiUrl) {
        break
    }
}

if ($uiUrl) {
    Start-Process $uiUrl
} else {
    Start-Process 'http://127.0.0.1:8085'
}
