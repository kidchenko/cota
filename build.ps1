<#
.SYNOPSIS
  Builds cota.exe.

.EXAMPLE
  .\build.ps1              # test + release binary
  .\build.ps1 -Run         # ... then launch it
  .\build.ps1 -Icons       # ... then render the icon faces to a contact sheet
  .\build.ps1 -SkipTests   # binary only
#>
[CmdletBinding()]
param(
  [switch]$Run,
  [switch]$Icons,
  [switch]$Panel,
  [switch]$Shot,
  [ValidateSet('dark','light')]
  [string]$Theme = 'dark',
  [ValidateRange(1,6)]
  [int]$Scale = 3,
  [switch]$SkipTests,
  [switch]$SkipInstaller
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Step($m) { Write-Host "`n==> $m" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "    $m" -ForegroundColor Green }
function Warn($m) { Write-Host "    $m" -ForegroundColor Yellow }

# Resolve cargo by PATH first, then by its known install location: a shell
# opened before rustup ran will not have it on PATH even though the persisted
# user PATH contains it.
function Find($name, $fallbacks) {
  $c = Get-Command $name -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  foreach ($f in $fallbacks) { if (Test-Path $f) { return $f } }
  return $null
}

$cargo = Find 'cargo' @("$env:USERPROFILE\.cargo\bin\cargo.exe")
if (-not $cargo) {
  Write-Host "cargo not found. Install the Rust MSVC toolchain:" -ForegroundColor Red
  Write-Host "  winget install Rustlang.Rustup"
  Write-Host "  rustup default stable-x86_64-pc-windows-msvc"
  exit 1
}

# A running instance holds a lock on the exe and the link step fails with a
# bare "Access is denied", which is a poor clue.
$running = Get-Process cota -ErrorAction SilentlyContinue
if ($running) {
  Warn "stopping $($running.Count) running instance(s) first"
  $running | Stop-Process -Force
  Start-Sleep -Milliseconds 500
}

if (-not $SkipTests) {
  Step 'Running tests'
  & $cargo test --quiet
  if ($LASTEXITCODE -ne 0) { throw "cargo test failed ($LASTEXITCODE)" }
  Ok 'tests passed'
}

Step 'Building release binary'
& $cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
$exe = Join-Path $PSScriptRoot 'target\release\cota.exe'
Ok ("cota.exe   {0:N2} MB" -f ((Get-Item $exe).Length / 1MB))

if ($Icons) {
  Step 'Rendering icon faces'
  $dir = Join-Path $PSScriptRoot 'target\icons'
  & $exe --dump-icons $dir
  $py = Find 'python' @()
  if ($py) {
    & $py (Join-Path $PSScriptRoot 'assets\preview.py') $dir
    Ok "contact sheets in $dir"
  } else {
    Warn "python not found; raw .rgba files are in $dir"
  }
}

if (-not $SkipInstaller) {
  $iscc = Find 'iscc' @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
  )
  if (-not $iscc) {
    Warn 'Inno Setup not found; skipping the installer.'
    Warn '  winget install JRSoftware.InnoSetup'
  } else {
    Step 'Building installer'
    # Version comes from Cargo.toml so there is one place to bump it.
    $cargoToml = Get-Content (Join-Path $PSScriptRoot 'Cargo.toml') -Raw
    $version = [regex]::Match($cargoToml, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    if (-not $version) { throw 'could not read version from Cargo.toml' }

    & $iscc "/DAppVersion=$version" (Join-Path $PSScriptRoot 'packaging\inno\cota.iss') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "iscc failed ($LASTEXITCODE)" }

    $setup = Join-Path $PSScriptRoot "dist\Cota-Setup-$version.exe"
    Ok ("Cota-Setup-$version.exe   {0:N2} MB" -f ((Get-Item $setup).Length / 1MB))
    Ok ("sha256  " + (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower())
  }
}

if ($Panel) {
  Step "Showing the panel ($Theme) for 20 seconds"
  Start-Process $exe -ArgumentList '--preview-panel', $Theme
  Ok 'pinned on screen - it will not dismiss when it loses focus'
}

if ($Shot) {
  # Rendered, never screenshotted. A real window sits over a real desktop and
  # DWM draws a shadow around it, so a capture arrives with whatever was behind
  # ghosted into the edges. This draws the same pixels onto a surface nothing
  # else has touched.
  Step "Rendering the landing-page shot (${Theme}, ${Scale}x)"
  $py = Find 'python' @()
  if (-not $py) { Warn 'python not found; cannot convert the raw output to PNG.'; return }

  $raw = Join-Path $PSScriptRoot 'target\panel.rgba'
  & $exe --render-panel $raw --theme $Theme --scale $Scale
  if ($LASTEXITCODE -ne 0) { throw "render failed ($LASTEXITCODE)" }
  & $py (Join-Path $PSScriptRoot 'assets\rgba2png.py') $raw (Join-Path $PSScriptRoot 'docs\img\panel.png')
  if ($LASTEXITCODE -ne 0) { throw "png conversion failed ($LASTEXITCODE)" }
  Ok 'docs\img\panel.png'
}

if ($Run) {
  Step 'Launching'
  Start-Process $exe
  Ok 'running in the tray'
}
