# Opens zflow on a fresh configuration and checks that it shows its window,
# starts the engine, and quits with the engine when asked. Leaves a picture of
# the screen and the app's log in -Out.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$App,
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$log = Join-Path $Out 'app.log'
$config = Join-Path $Out 'config/config.toml'
$env:RUST_LOG = 'warn,zflow=debug'
# The app is found by name below, which also covers a launcher that hands
# its arguments over and exits.
Start-Process -FilePath $App -ArgumentList '--config', "`"$config`"", '--show-settings' -RedirectStandardError $log
Start-Sleep -Seconds 20

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$picture = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
[System.Drawing.Graphics]::FromImage($picture).CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$picture.Save((Join-Path $Out 'screen.png'))

$windows = @(Get-Process Zflow.App -ErrorAction SilentlyContinue | Where-Object MainWindowTitle -eq 'zflow')
$engines = @(Get-Process zflow -ErrorAction SilentlyContinue)
Write-Host "zflow windows: $($windows.Count), engines: $($engines.Count)"
& $App --quit
$quit = $true
foreach ($process in @($windows) + @($engines)) {
    if (-not $process.WaitForExit(15000)) {
        $quit = $false
        Stop-Process -Id $process.Id -Force
    }
}
if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log }

if ($windows.Count -ne 1) { throw "Expected one zflow window, found $($windows.Count)" }
if ($engines.Count -lt 1) { throw 'The app did not start the engine' }
if (-not $quit) { throw 'The app or its engine did not quit when asked' }
if ((Test-Path -LiteralPath $log) -and (Select-String -LiteralPath $log -Pattern panicked -Quiet)) { throw 'The app panicked' }
Write-Host 'zflow opened, started the engine and quit'
