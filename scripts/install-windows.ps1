[CmdletBinding()]
param([string]$SourceDirectory, [switch]$NoLaunch)
$ErrorActionPreference = 'Stop'
if (-not $SourceDirectory) {
    $SourceDirectory = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'Zflow.App.exe')) { $PSScriptRoot } else { Join-Path (Split-Path -Parent $PSScriptRoot) 'windows/dist' }
}
$source = (Resolve-Path -LiteralPath $SourceDirectory).Path
foreach ($file in @('Zflow.App.exe', 'zflow.exe', 'Microsoft.UI.Xaml.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $source $file))) { throw "The complete Windows app is required; missing $file in $source" }
}
$destination = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs/zflow'))
if ($source.TrimEnd('\') -eq $destination.TrimEnd('\')) { throw 'Run the installer from the extracted download or build output.' }
$running = Get-CimInstance Win32_Process -Filter "Name = 'Zflow.App.exe' OR Name = 'zflow.exe'" | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($destination + '\', [StringComparison]::OrdinalIgnoreCase) }
if ($running) { throw 'Quit the installed zflow from its notification-area menu, then run this installer again.' }
New-Item -ItemType Directory -Path $destination -Force | Out-Null
Get-ChildItem -LiteralPath $source | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $destination -Recurse -Force }
$programs = [Environment]::GetFolderPath('Programs')
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut((Join-Path $programs 'zflow.lnk'))
$shortcut.TargetPath = Join-Path $destination 'Zflow.App.exe'
$shortcut.WorkingDirectory = $destination
$shortcut.Description = 'Share your keyboard and pointer between computers'
$shortcut.Save()
Write-Host "Installed zflow for this user at $destination"
Write-Host 'Your configuration and paired computers remain in %LOCALAPPDATA%\zflow.'
if (-not $NoLaunch) { Start-Process -FilePath (Join-Path $destination 'Zflow.App.exe') }
