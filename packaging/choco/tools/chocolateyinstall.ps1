$ErrorActionPreference = 'Stop'

$version  = '0.0.1'
$url      = "https://github.com/kidchenko/cota/releases/download/v$version/Cota-Setup-$version.exe"

$packageArgs = @{
  packageName    = 'cota'
  fileType       = 'EXE'
  url64bit       = $url
  # Replaced by the release workflow, which computes it from the built artifact.
  checksum64     = '47a89bcdb535fae6fd0f845164c7873b3e34af77edc6094149392009c5a0ad38'
  checksumType64 = 'sha256'
  # Inno Setup silent switches. /NORESTART because nothing here needs a reboot.
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
  softwareName   = 'Cota*'
}

Install-ChocolateyPackage @packageArgs

Write-Host ''
Write-Host 'Cota reads the token Claude Code already stores.' -ForegroundColor Cyan
Write-Host 'If you have never signed in on this machine, run `claude` once and' -ForegroundColor Cyan
Write-Host 'the tray icon will pick it up on its next poll.' -ForegroundColor Cyan
