<#
.SYNOPSIS
  Install FastTTS for the current user, or remove it.

.DESCRIPTION
  Copies the self-contained fasttts.exe to %LOCALAPPDATA%\Programs\FastTTS,
  adds Desktop and Start menu shortcuts and starts it. It builds the release
  first, so the installed exe always matches the source.

.EXAMPLE
  .\install.ps1            # install and start
  .\install.ps1 -Startup   # also start FastTTS when you sign in (until -Uninstall)
  .\install.ps1 -Uninstall
#>
param(
  [switch]$Startup,
  [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$target = Join-Path $env:LOCALAPPDATA 'Programs\FastTTS'
$exe = Join-Path $target 'fasttts.exe'
$links = @(
  (Join-Path ([Environment]::GetFolderPath('Desktop')) 'FastTTS.lnk'),
  (Join-Path ([Environment]::GetFolderPath('Programs')) 'FastTTS.lnk'),
  (Join-Path ([Environment]::GetFolderPath('Startup')) 'FastTTS.lnk')
)

function Stop-FastTTS {
  $running = Get-Process fasttts -ErrorAction SilentlyContinue
  if ($running) {
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
  }
}

if ($Uninstall) {
  Stop-FastTTS
  $links | Where-Object { Test-Path $_ } | Remove-Item -Force
  if (Test-Path $target) { Remove-Item $target -Recurse -Force }
  Write-Host 'FastTTS removed. Settings and logs stay in %APPDATA%\FastTTS and %LOCALAPPDATA%\FastTTS.'
  return
}

Push-Location $PSScriptRoot
try {
  cargo build --release
  if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }
} finally {
  Pop-Location
}
$built = Join-Path $PSScriptRoot 'target\x86_64-pc-windows-gnullvm\release\fasttts.exe'
Stop-FastTTS
New-Item -ItemType Directory -Force $target | Out-Null
Copy-Item $built $exe -Force

$shell = New-Object -ComObject WScript.Shell
$wanted = if ($Startup) { $links } else { $links[0..1] }
foreach ($path in $wanted) {
  $link = $shell.CreateShortcut($path)
  $link.TargetPath = $exe
  $link.WorkingDirectory = $target
  $link.IconLocation = "$exe,0"
  $link.Description = 'FastTTS - hold Caps Lock to dictate'
  $link.Save()
}

Start-Process $exe
Write-Host "FastTTS installed to $target and started."
