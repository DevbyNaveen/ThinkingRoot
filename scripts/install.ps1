$ErrorActionPreference = 'Stop'

$InstallDir = "$env:USERPROFILE\.local\bin"
$BinName = "root.exe"
$Artifact = "root-windows-amd64.exe"
$RepoUrl = "https://github.com/DevbyNaveen/releases/releases/latest/download/$Artifact"

Write-Host "ThinkingRoot universal installer for Windows" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Cyan

# 1. Create install directory
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Write-Host "Created directory $InstallDir"
}

# 2. Download artifact
$DestPath = Join-Path $InstallDir $BinName
Write-Host "Downloading $Artifact..."
Invoke-WebRequest -Uri $RepoUrl -OutFile $DestPath
Write-Host "✅ Downloaded binary to $DestPath" -ForegroundColor Green

# 3. Update PATH permanently
$UserPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)

if ($UserPath -notlike "*$InstallDir*") {
    $NewPath = $UserPath
    if (-not $NewPath.EndsWith(";")) {
        $NewPath += ";"
    }
    $NewPath += $InstallDir
    
    [Environment]::SetEnvironmentVariable("Path", $NewPath, [EnvironmentVariableTarget]::User)
    Write-Host "✅ Added $InstallDir to your PATH" -ForegroundColor Green
    Write-Host "⚠️  IMPORTANT: Please restart your terminal window so the new PATH takes effect!" -ForegroundColor Yellow
} else {
    Write-Host "✅ $InstallDir is already in your PATH" -ForegroundColor Green
}

Write-Host "🚀 ThinkingRoot installation complete! Once you restart your terminal, run 'root --help' to get started." -ForegroundColor Cyan
