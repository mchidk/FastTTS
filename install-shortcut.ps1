param(
  [string]$DesktopPath = [Environment]::GetFolderPath('Desktop')
)

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $root 'target\x86_64-pc-windows-gnu\release\fasttts.exe'
$icon = Join-Path $root 'assets\fasttts.ico'

if (-not (Test-Path $exe)) {
  Write-Host "Build FastTTS first: cargo build --release"
  exit 1
}

$dir = Split-Path -Parent $exe
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item (Join-Path $root 'runtime\*.dll') $dir -Force

$shortcut = Join-Path $DesktopPath 'FastTTS.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = $exe
$link.WorkingDirectory = $root
if (Test-Path $icon) { $link.IconLocation = "$icon,0" }
$link.Description = 'FastTTS - hold Caps Lock to dictate'
$link.Save()
Write-Host "Created $shortcut"
