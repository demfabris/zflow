[CmdletBinding()]
param([ValidateSet('Debug','Release')][string]$Configuration = 'Release', [switch]$Launch)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$localSdk = Join-Path $env:LOCALAPPDATA 'zflow-dev/dotnet/dotnet.exe'
$dotnet = if (Test-Path -LiteralPath $localSdk) { $localSdk } else { (Get-Command dotnet -ErrorAction Stop).Source }
Push-Location $repo
try {
    & cargo build --locked --release --bin zflow
    if ($LASTEXITCODE -ne 0) { throw 'Rust engine build failed' }
    $output = Join-Path $repo 'windows/dist'
    & $dotnet publish windows/Zflow.App/Zflow.App.csproj -c $Configuration -p:Platform=x64 --output $output
    if ($LASTEXITCODE -ne 0) { throw 'WinUI app build failed' }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-windows.ps1') -Destination $output
    Copy-Item -LiteralPath (Join-Path $repo 'windows/README.md') -Destination $output
    Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $output
    Write-Host "Built zflow in $output"
    if ($Launch) { Start-Process -FilePath (Join-Path $output 'Zflow.App.exe') }
} finally { Pop-Location }
