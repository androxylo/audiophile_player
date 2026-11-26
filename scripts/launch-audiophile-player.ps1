<#
.SYNOPSIS
    Launches Audiophile Player (if not already running) and opens its local HTTP control UI.

.DESCRIPTION
    - If audiophile-player.exe is already running, checks %LOCALAPPDATA%\audiophile-player\server.url
      and queries the running process's bound localhost TCP listener directly via Get-NetTCPConnection
      (avoiding slow 1-2 s closed-port SYN retries) so the active Web Control UI opens immediately
      without disturbing playback or launching a duplicate instance.
    - Otherwise, starts target\release\audiophile-player.exe in a clean console window (close or
      press Ctrl+C to stop and release the DAC), polls server.url + its bound listener port every
      200 ms for up to 6 seconds, and opens the UI in the default browser.
#>

$ErrorActionPreference = 'SilentlyContinue'

$repoRoot    = Split-Path -Parent $PSScriptRoot
$releaseExe  = Join-Path $repoRoot 'audiophile-player.exe'
$cargoExe    = Join-Path $repoRoot 'target\release\audiophile-player.exe'
$exePath     = if (Test-Path -LiteralPath $releaseExe) { $releaseExe } else { $cargoExe }
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

function Find-PlayerUiUrlFast([int[]]$ProcessIds) {
    # 1. Primary path: read the runtime URL written by the active player instance.
    if (Test-Path -LiteralPath $urlFilePath) {
        $recorded = (Get-Content -LiteralPath $urlFilePath -Raw)
        if ($recorded -and (Test-PlayerUiUrl $recorded)) {
            return $recorded.Trim().TrimEnd('/')
        }
    }

    # 2. Fast OS socket-table lookup for the player's actual bound localhost listener port(s)
    #    (zero closed-port TCP SYN retry latency).
    if ($ProcessIds -and $ProcessIds.Count -gt 0) {
        foreach ($pidItem in $ProcessIds) {
            $listeners = Get-NetTCPConnection -State Listen -LocalAddress '127.0.0.1' -OwningProcess $pidItem -ErrorAction SilentlyContinue
            foreach ($conn in $listeners) {
                if ($conn.LocalPort -ge 8085 -and $conn.LocalPort -le 8199) {
                    $candidate = "http://127.0.0.1:$($conn.LocalPort)"
                    if (Test-PlayerUiUrl $candidate) {
                        return $candidate
                    }
                }
            }
        }
    }

    return $null
}

function Find-PlayerUiUrlFallbackScan {
    # Final fallback only if both server.url and Get-NetTCPConnection failed to identify the port.
    # Scans the full server port range (8085..8199) matching ap-control's DEFAULT_PORT_START..END.
    for ($port = 8085; $port -le 8199; $port++) {
        $tcp = New-Object System.Net.Sockets.TcpClient
        $connected = $false
        try {
            $iar = $tcp.BeginConnect('127.0.0.1', $port, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(25, $false)) {
                $tcp.EndConnect($iar)
                $connected = $true
            }
        } catch {
            $connected = $false
        } finally {
            $tcp.Dispose()
        }
        if ($connected) {
            $candidate = "http://127.0.0.1:$port"
            if (Test-PlayerUiUrl $candidate) {
                return $candidate
            }
        }
    }
    return $null
}

if (-not (Test-Path -LiteralPath $exePath)) {
    Show-PlayerWarning "audiophile-player.exe was not found at:`n$exePath`n`nBuild it first with:`ncargo build --release --bin audiophile-player"
    exit 1
}

# 1. If already running, NEVER launch a second instance (which would fail to lock the DAC).
$running = @(Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    $pids = @($running | ForEach-Object { $_.Id })
    $url = Find-PlayerUiUrlFast $pids
    if (-not $url) {
        $url = Find-PlayerUiUrlFallbackScan
    }
    if ($url) {
        Start-Process $url
        exit 0
    }
    Show-PlayerWarning "audiophile-player.exe is already running, but its localhost Web Control UI did not respond on ports 8085..8199.`n`nCheck the player console window instead of starting a duplicate instance."
    exit 1
}

# 2. Launch audiophile-player.exe in a normal console window so status is visible and Ctrl+C/close stops cleanly.
$proc = Start-Process -FilePath $exePath -WorkingDirectory $repoRoot -PassThru

# 3. Poll server.url and the spawned process's bound localhost port every 200 ms for up to 6 seconds.
$deadline = (Get-Date).AddSeconds(6)
$uiUrl = $null
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    $uiUrl = Find-PlayerUiUrlFast @($proc.Id)
    if ($uiUrl) {
        break
    }
    if ($proc.HasExited) {
        break
    }
}
if (-not $uiUrl -and -not $proc.HasExited) {
    $uiUrl = Find-PlayerUiUrlFallbackScan
}

if ($uiUrl) {
    Start-Process $uiUrl
} else {
    Show-PlayerWarning "audiophile-player.exe was started, but its localhost Web Control UI did not respond within 6 seconds (ports 8085..8199).`n`nCheck the console window for DAC or configuration errors."
    exit 1
}
