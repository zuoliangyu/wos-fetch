# dev.ps1 — start wos-fetch in Tauri dev mode (hot-reload frontend + backend)
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] pnpm is not installed or not on PATH." -ForegroundColor Red
    Write-Host "        Install Node.js 20+ and run 'corepack enable' first."
    Write-Host "        See: https://pnpm.io/installation"
    exit 1
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "[ERROR] cargo (Rust toolchain) is not installed or not on PATH." -ForegroundColor Red
    Write-Host "        Install Rust from https://rustup.rs/"
    exit 1
}

if (-not (Test-Path "node_modules")) {
    Write-Host "[SETUP] Installing JS dependencies (one-time)..." -ForegroundColor Cyan
    pnpm install
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[ERROR] pnpm install failed." -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

Write-Host ""
Write-Host "[DEV] Starting Tauri dev mode (Ctrl+C to stop)..." -ForegroundColor Green
Write-Host ""
pnpm tauri dev
