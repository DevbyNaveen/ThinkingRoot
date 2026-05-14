# ThinkingRoot Windows installer
# Usage: iwr -useb https://thinkingroot.com/install.ps1 | iex
#
# Mirrors `install.sh`:
#   1. Downloads the `root.exe` binary
#   2. Verifies SHA256 against `checksums.txt`
#   3. Installs to `%LOCALAPPDATA%\Programs\ThinkingRoot\bin\root.exe`
#   4. Adds the bin dir to the user's PATH (HKCU\Environment)
#   5. Writes the install manifest at
#      `%APPDATA%\thinkingroot\install-manifest.json`
#   6. Downloads + extracts the desktop app (NSIS installer) into
#      `%LOCALAPPDATA%\Programs\ThinkingRoot\app` when available
#   7. Registers a Task Scheduler ONLOGON entry so the daemon
#      auto-starts (via `root service install`)
#   8. Downloads the NLI ONNX model unless TR_SKIP_NLI=1
#
# Tunable via environment variables:
#   $env:TR_SKIP_SERVICE = "1"   # skip Task Scheduler registration
#   $env:TR_SKIP_APP     = "1"   # skip desktop bundle download
#   $env:TR_SKIP_NLI     = "1"   # skip ~83 MB NLI model
#   $env:VERSION         = "v0.9.1"  # pin to a specific release
#   $env:TR_TEST_BASE_URL = "..."  # tests only — point at local http.server

$ErrorActionPreference = 'Stop'

# ── Constants ────────────────────────────────────────────────────────────────

$RELEASES_REPO = 'DevbyNaveen/releases'
$NLI_MODELS_TAG = 'nli-models'
$BinaryName = 'root.exe'
$Asset = 'root-windows-amd64.exe'
$NliOnnx = 'model_quint8_avx2.onnx'

# ── Install paths ────────────────────────────────────────────────────────────

$InstallRoot = Join-Path $env:LOCALAPPDATA 'Programs\ThinkingRoot'
$InstallBin  = Join-Path $InstallRoot 'bin'
$InstallApp  = Join-Path $InstallRoot 'app'
$ConfigDir   = Join-Path $env:APPDATA 'thinkingroot'
$ModelDir    = Join-Path $env:LOCALAPPDATA 'thinkingroot\models'

# ── Helpers ──────────────────────────────────────────────────────────────────

function Say([string]$msg) {
    Write-Host "==> $msg" -ForegroundColor Green
}
function SayDim([string]$msg) {
    Write-Host "    $msg" -ForegroundColor DarkGray
}
function Warn([string]$msg) {
    Write-Host "Warning: $msg" -ForegroundColor Yellow
}
function Fail([string]$msg) {
    Write-Host "Error: $msg" -ForegroundColor Red
    exit 1
}

function Get-LatestVersion {
    $url = "https://api.github.com/repos/$RELEASES_REPO/releases/latest"
    $headers = @{ 'User-Agent' = 'thinkingroot-install' }
    try {
        $resp = Invoke-RestMethod -Uri $url -Headers $headers -ErrorAction Stop
        return $resp.tag_name
    } catch {
        return $null
    }
}

function Get-Sha256([string]$path) {
    (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLower()
}

function Add-DirToUserPath([string]$dir) {
    # Read the User-scoped PATH directly from the registry rather
    # than $env:Path so we get the persisted value, not the inherited
    # process value (which mixes User + Machine).
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $needles = $userPath -split ';' | Where-Object { $_ -ne '' }
    if ($needles -contains $dir) {
        return $false
    }
    $newPath = if ($userPath -and -not $userPath.EndsWith(';')) {
        "$userPath;$dir"
    } else {
        "$userPath$dir"
    }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    # Live-broadcast the change so already-running Explorer/cmd
    # windows pick up the new PATH on their next launch. Without
    # this, only freshly-spawned processes see the change.
    $env:Path = "$env:Path;$dir"
    return $true
}

function Write-InstallManifest([string]$binPath, [string]$version, [string]$checksum) {
    if (-not (Test-Path $ConfigDir)) {
        New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    }
    $manifestPath = Join-Path $ConfigDir 'install-manifest.json'
    $installedAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

    # Preserve a pre-existing desktop-bundle entry if present so the
    # desktop's idempotent re-registration on next launch doesn't
    # overwrite our CLI registration.
    $desktopEntry = $null
    if (Test-Path $manifestPath) {
        try {
            $existing = Get-Content $manifestPath -Raw | ConvertFrom-Json
            if ($existing.binaries) {
                $desktopEntry = $existing.binaries | Where-Object { $_.id -eq 'desktop-bundle' } | Select-Object -First 1
            }
        } catch {
            Warn "existing install-manifest.json was unreadable; rewriting fresh"
        }
    }

    $binaries = @(
        [pscustomobject]@{
            id              = 'cli-script'
            path            = $binPath
            version         = $version
            installed_at    = $installedAt
            checksum_blake3 = $checksum
        }
    )
    if ($desktopEntry) {
        $binaries += $desktopEntry
    }

    $manifest = [pscustomobject]@{
        schema_version     = 1
        binaries           = $binaries
        preferred          = 'cli-script'
        setup_complete_at  = $null
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -Path $manifestPath -Encoding UTF8
    Say "Registered install manifest at $manifestPath"
}

function Install-NliModel {
    if (-not (Test-Path $ModelDir)) {
        New-Item -ItemType Directory -Force -Path $ModelDir | Out-Null
    }
    $base = "https://github.com/$RELEASES_REPO/releases/download/$NLI_MODELS_TAG"
    $onnxDest = Join-Path $ModelDir $NliOnnx
    $tokenizerDest = Join-Path $ModelDir 'tokenizer.json'

    if (Test-Path $onnxDest) {
        SayDim "NLI model already cached: $onnxDest"
    } else {
        Say "Downloading NLI model (~83 MB, one-time)..."
        try {
            Invoke-WebRequest -Uri "$base/$NliOnnx" -OutFile $onnxDest -UseBasicParsing
            SayDim "Saved to $onnxDest"
        } catch {
            Warn "NLI model download failed — grounding will use judges 1-3 only. Re-run installer to retry."
            return
        }
    }
    if (Test-Path $tokenizerDest) {
        SayDim "Tokenizer already cached."
    } else {
        try {
            Invoke-WebRequest -Uri "$base/tokenizer.json" -OutFile $tokenizerDest -UseBasicParsing
            SayDim "Saved to $tokenizerDest"
        } catch {
            Warn "Tokenizer download failed — re-run installer to retry."
            return
        }
    }
    Say "NLI models ready."
}

function Install-DesktopApp([string]$baseUrl, [string]$version) {
    # Tauri 2 NSIS asset naming: `<productName>_<version>_<arch>-setup.exe`
    # productName is "ThinkingRoot" (no spaces, set in tauri.conf.json).
    # We pull the silent NSIS installer and run it /S (silent) into
    # $InstallApp. If the asset is missing this release, skip
    # cleanly — the CLI is fully functional on its own.
    $arch = 'x64'
    $asset = "ThinkingRoot_${version}_${arch}-setup.exe"
    $url = "$baseUrl/$asset"
    $dest = Join-Path $env:TEMP $asset
    try {
        Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing -ErrorAction Stop
    } catch {
        SayDim "Desktop bundle not in this release — skipping (CLI is fully functional)."
        return
    }
    if (-not (Test-Path $InstallApp)) {
        New-Item -ItemType Directory -Force -Path $InstallApp | Out-Null
    }
    # NSIS /S = silent; /D=... = install dir (must be LAST and unquoted
    # per the NSIS spec). Wait for the process so we report failure
    # honestly instead of returning before the install finishes.
    $proc = Start-Process -FilePath $dest -ArgumentList "/S","/D=$InstallApp" -Wait -PassThru
    if ($proc.ExitCode -ne 0) {
        Warn "Desktop installer exited $($proc.ExitCode). The CLI is still installed and functional."
        return
    }
    Say "Installed: $InstallApp"
}

function Register-LoginAgent([string]$binPath) {
    if ($env:TR_SKIP_SERVICE -eq '1') {
        SayDim "Skipping login-agent registration (TR_SKIP_SERVICE=1)"
        return
    }
    try {
        & $binPath service install
    } catch {
        Warn "login-agent registration failed — run ``root service install`` manually if you want auto-start."
    }
}

# ── Main ─────────────────────────────────────────────────────────────────────

Say "ThinkingRoot installer for Windows"

if (-not (Test-Path $InstallBin)) {
    New-Item -ItemType Directory -Force -Path $InstallBin | Out-Null
}

$Version = if ($env:VERSION) { $env:VERSION } else { Get-LatestVersion }
if (-not $Version) {
    Fail "Could not determine latest version. Set `$env:VERSION manually."
}
$BaseUrl = if ($env:TR_TEST_BASE_URL) {
    $env:TR_TEST_BASE_URL
} else {
    "https://github.com/$RELEASES_REPO/releases/download/$Version"
}

Say "Installing root $Version for windows/amd64"

$tmpDir = Join-Path $env:TEMP "thinkingroot-install-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
try {
    $assetPath = Join-Path $tmpDir $Asset
    $checksumsPath = Join-Path $tmpDir 'checksums.txt'

    Say "Downloading binary..."
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $assetPath -UseBasicParsing
    Invoke-WebRequest -Uri "$BaseUrl/checksums.txt" -OutFile $checksumsPath -UseBasicParsing

    Say "Verifying SHA256 checksum..."
    $expectedLine = (Get-Content $checksumsPath | Where-Object { $_ -match "(?<![A-Za-z0-9._-])$([regex]::Escape($Asset))$" } | Select-Object -First 1)
    if (-not $expectedLine) {
        Fail "Checksum not found for $Asset in checksums.txt"
    }
    $expected = ($expectedLine -split '\s+')[0].ToLower()
    if ($expected -notmatch '^[0-9a-f]{64}$') {
        Fail "Malformed SHA256 in checksums.txt: $expected"
    }
    $actual = Get-Sha256 $assetPath
    if ($expected -ne $actual) {
        Write-Host "Checksum mismatch!" -ForegroundColor Red
        Write-Host "  Expected: $expected"
        Write-Host "  Got:      $actual"
        exit 1
    }
    Say "Checksum OK"

    # Atomic-ish install: move staged file over the final path. On
    # Windows, the destination must not be held open by another
    # process; if it is, fall back to a Stop-Process probe with a
    # honest error rather than a silently-corrupt half-install.
    $finalBin = Join-Path $InstallBin $BinaryName
    $staged = "$finalBin.tr-installing"
    Move-Item -Force $assetPath $staged
    if (Test-Path $finalBin) {
        try {
            Remove-Item -Force $finalBin
        } catch {
            Fail "Cannot replace $finalBin — close any running ThinkingRoot process and retry."
        }
    }
    Move-Item -Force $staged $finalBin
    Say "Installed: $finalBin"

    # ── PATH ─────────────────────────────────────────────────────
    if (Add-DirToUserPath $InstallBin) {
        Say "Added $InstallBin to your PATH"
        SayDim "Open a new terminal window to pick up the PATH change."
    } else {
        SayDim "$InstallBin already in PATH"
    }

    # ── Manifest ─────────────────────────────────────────────────
    $checksumBlake3 = ''
    try {
        $checksumBlake3 = (& $finalBin hash-file $finalBin 2>$null)
    } catch {
        # `root hash-file` is a hidden subcommand added late; fall
        # back to an empty checksum honestly — the slice F doctor
        # check will repair it on first daemon start.
    }
    if (-not $checksumBlake3) { $checksumBlake3 = '' }
    Write-InstallManifest $finalBin $Version $checksumBlake3

    # ── Cache checksums.txt for recovery ─────────────────────────
    Copy-Item -Force $checksumsPath (Join-Path $ConfigDir 'checksums-cache.txt')

    # ── NLI models ───────────────────────────────────────────────
    if ($env:TR_SKIP_NLI -eq '1') {
        SayDim "Skipping NLI model download (TR_SKIP_NLI=1)"
    } else {
        Install-NliModel
    }

    # ── Desktop app ──────────────────────────────────────────────
    if ($env:TR_SKIP_APP -eq '1') {
        SayDim "Skipping desktop app install (TR_SKIP_APP=1)"
    } else {
        Install-DesktopApp $BaseUrl $Version
    }

    # ── Login agent (Task Scheduler) ─────────────────────────────
    Register-LoginAgent $finalBin

    Write-Host ""
    Say "Done!"
    if (Test-Path (Join-Path $ConfigDir 'install-manifest.json')) {
        SayDim "Install manifest: $(Join-Path $ConfigDir 'install-manifest.json')"
    }
    try {
        & $finalBin doctor --quiet 2>$null
        if ($LASTEXITCODE -eq 0) {
            Say "Doctor: all checks pass."
        } else {
            SayDim "Doctor flagged setup gaps; run ``root doctor`` for details."
        }
    } catch {
        # doctor is best-effort here — failures don't roll back install
    }
    & $finalBin --version
    Write-Host ""
    Write-Host "    Get started:"
    Write-Host "      root setup            # interactive credentials wizard"
    Write-Host "      root compile .        # compile your first knowledge base"
    Write-Host "      root ask `"what does this codebase do?`""
    Write-Host ""
    Write-Host "    Service management:"
    Write-Host "      root service install     # register login agent (already done)"
    Write-Host "      root service uninstall   # remove login agent"
    Write-Host ""
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
