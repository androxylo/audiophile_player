<#
.SYNOPSIS
    Launches Audiophile Player (if not already running) and opens its local HTTP control UI.

.DESCRIPTION
    - If audiophile-player.exe is already running, checks %LOCALAPPDATA%\audiophile-player\server.url
      and scans the full localhost port range (8085..8199) to open the active Web Control UI
      directly in the default browser without disturbing playback or launching a duplicate instance.
    - Otherwise, starts target\release\audiophile-player.exe in a clean console window (close or
      press Ctrl+C to stop and release the DAC), waits for the HTTP UI to come up, and opens it.
#>

$ErrorActionPreference = 'SilentlyContinue'

$repoRoot    = Split-Path -Parent $PSScriptRoot
$exePath     = Join-Path $repoRoot 'target\release\audiophile-player.exe'
$urlFilePath = Join-Path $env:LOCALAPPDATA 'audiophile-player\server.url'

function Show-PlayerWarning([string]$Message) {
    Add-Type -AssemblyName PresentationFramework
    [System.Windows.MessageBox]::Show(
        $Message,
        "Audiophile Player",
        [System.Windows.MessageBoxButton]::OK,
        [System.Windows.MessageBoxImage]::Warning
    ) | Out-Null
}

function Test-PlayerUiUrl([string]$BaseUrl) {
    if (-not $BaseUrl) { return $false }
    $trimmed = $BaseUrl.Trim().TrimEnd('/')
    if ($trimmed -notmatch '^http://127\.0\.0\.1:(\d+)$') {
        return $false
    }
    $port = [int]$Matches[1]
    $tcp = New-Object System.Net.Sockets.TcpClient
    try {
        $iar = $tcp.BeginConnect('127.0.0.1', $port, $null, $null)
        if (-not $iar.AsyncWaitHandle.WaitOne(40, $false)) {
            return $false
        }
        $tcp.EndConnect($iar)
    } catch {
        return $false
    } finally {
        $tcp.Dispose()
    }

    try {
        $req = [System.Net.HttpWebRequest]::Create("$trimmed/api/status")
        $req.Timeout = 300
        $req.ReadWriteTimeout = 300
        $resp = $req.GetResponse()
        try {
            $reader = New-Object System.IO.StreamReader($resp.GetResponseStream())
            $body = $reader.ReadToEnd()
            $reader.Dispose()
            return ($body -match '"dac"\s*:' -and $body -match '"mode"\s*:')
        } finally {
            $resp.Close()
        }
    } catch {
        return $false
    }
}

function Find-PlayerUiUrl {
    # 1. Fast path: read the runtime URL written by the active player instance.
    if (Test-Path -LiteralPath $urlFilePath) {
        $recorded = (Get-Content -LiteralPath $urlFilePath -Raw)
        if ($recorded -and (Test-PlayerUiUrl $recorded)) {
            return $recorded.Trim().TrimEnd('/')
        }
    }

    # 2. Full fallback scan across DEFAULT_PORT_START..DEFAULT_PORT_END (8085..8199).
    for ($port = 8085; $port -le 8199; $port++) {
        $candidate = "http://127.0.0.1:$port"
        if (Test-PlayerUiUrl $candidate) {
            return $candidate
        }
    }
    return $null
}

if (-not (Test-Path -LiteralPath $exePath)) {
    Show-PlayerWarning "audiophile-player.exe was not found at:`n$exePath`n`nBuild it first with:`ncargo build --release --bin audiophile-player"
    exit 1
}

# 1. If already running, NEVER launch a second instance (which would fail to lock the DAC).
$running = Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue
if ($running) {
    $url = Find-PlayerUiUrl
    if ($url) {
        Start-Process $url
        exit 0
    }
    Show-PlayerWarning "audiophile-player.exe is already running, but its localhost Web Control UI did not respond on ports 8085..8199.`n`nCheck the player console window instead of starting a duplicate instance."
    exit 1
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
    Show-PlayerWarning "audiophile-player.exe was started, but its localhost Web Control UI did not respond within 6 seconds (ports 8085..8199).`n`nCheck the console window for DAC or configuration errors."
    exit 1
}
