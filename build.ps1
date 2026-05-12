# build.ps1 — produce a release-mode wos-fetch.exe plus MSI + NSIS installers
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] pnpm is not installed or not on PATH." -ForegroundColor Red
    Write-Host "        Install Node.js 20+ and run 'corepack enable' first."
    exit 1
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] cargo (Rust toolchain) is not installed or not on PATH." -ForegroundColor Red
    Write-Host "        Install Rust from https://rustup.rs/"
    exit 1
}

if (-not (Test-Path "node_modules")) {
    Write-Host "[SETUP] Installing JS dependencies..." -ForegroundColor Cyan
    pnpm install
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

Write-Host ""
Write-Host "[BUILD] Running 'pnpm tauri build' (release + installers)..." -ForegroundColor Green
Write-Host "        First build can take 5-10 minutes; incremental builds are fast."
Write-Host ""
pnpm tauri build
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[DONE] Artifacts:" -ForegroundColor Green
$exe = "src-tauri\target\release\wos-fetch.exe"
$msiDir = "src-tauri\target\release\bundle\msi"
$nsisDir = "src-tauri\target\release\bundle\nsis"
if (Test-Path $exe) { Write-Host "  exe : $exe" }
if (Test-Path $msiDir) {
    Get-ChildItem $msiDir -Filter "*.msi" | ForEach-Object { Write-Host "  msi : $($_.FullName)" }
}
if (Test-Path $nsisDir) {
    Get-ChildItem $nsisDir -Filter "*-setup.exe" | ForEach-Object { Write-Host "  nsis: $($_.FullName)" }
}
