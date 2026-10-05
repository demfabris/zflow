[CmdletBinding()]
param([string]$SignParameters)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
& (Join-Path $PSScriptRoot 'build-windows.ps1')
$manifest = Get-Content -LiteralPath (Join-Path $repo 'Cargo.toml') -Raw
$version = [regex]::Match($manifest, '(?m)^version = "([^"]+)"').Groups[1].Value
if (-not $version) { throw 'Cargo version is missing' }
if ($env:GITHUB_REF -like 'refs/tags/*' -and $env:GITHUB_REF_NAME -ne "v$version") { throw 'The tag must match Cargo.toml' }
$output = Join-Path $repo 'target/windows-release'
if (Test-Path -LiteralPath $output) { Remove-Item -LiteralPath $output -Recurse -Force }
New-Item -ItemType Directory -Path $output -Force | Out-Null
$localSdk = Join-Path $env:LOCALAPPDATA 'zflow-dev/dotnet/dotnet.exe'
$dotnet = if (Test-Path -LiteralPath $localSdk) { $localSdk } else { (Get-Command dotnet -ErrorAction Stop).Source }
$tools = Join-Path $repo 'target/velopack-tools'
if (Test-Path -LiteralPath $tools) { Remove-Item -LiteralPath $tools -Recurse -Force }
& $dotnet tool install vpk --version 1.2.161 --tool-path $tools
if ($LASTEXITCODE -ne 0) { throw 'Velopack tool installation failed' }
$pack = @('pack', '--packId', 'Zflow.App', '--packVersion', $version,
    '--packDir', (Join-Path $repo 'windows/dist'), '--mainExe', 'Zflow.App.exe',
    '--packTitle', 'zflow', '--packAuthors', 'zflow contributors', '--channel', 'win-x64',
    '--runtime', 'win-x64', '--shortcuts', 'StartMenuRoot', '--noPortable',
    '--icon', (Join-Path $repo 'assets/windows/zflow.ico'),
    '--releaseNotes', (Join-Path $repo 'packaging/RELEASE_NOTES.md'), '--outputDir', $output, '--skip-updates')
if ($SignParameters) { $pack += @('--signParams', $SignParameters) }
else { Write-Warning 'Windows artifacts are unsigned. Configure -SignParameters with a trusted code-signing certificate for signed releases.' }
$previousDotnetRoot = $env:DOTNET_ROOT
try {
    $env:DOTNET_ROOT = Split-Path -Parent $dotnet
    & (Join-Path $tools 'vpk.exe') @pack
    if ($LASTEXITCODE -ne 0) { throw 'Velopack packaging failed' }
} finally { $env:DOTNET_ROOT = $previousDotnetRoot }
$setup = @(Get-ChildItem -LiteralPath $output -Filter '*-Setup.exe')
if ($setup.Count -ne 1) { throw 'Expected exactly one Windows installer' }
Move-Item -LiteralPath $setup[0].FullName -Destination (Join-Path $output 'zflow-windows-x86_64-Setup.exe')
$feed = Get-Content -LiteralPath (Join-Path $output 'releases.win-x64.json') -Raw | ConvertFrom-Json
$full = @($feed.Assets | Where-Object { $_.PackageId -eq 'Zflow.App' -and $_.Version -eq $version -and $_.Type -eq 'Full' })
if ($full.Count -ne 1 -or -not (Test-Path -LiteralPath (Join-Path $output $full[0].FileName))) { throw 'The Windows update feed must reference the packaged full release' }
# Publication uses the feed and full package directly, not Velopack upload commands.
Get-ChildItem -LiteralPath $output -Filter 'assets.*.json' | Remove-Item
Remove-Item -LiteralPath (Join-Path $output 'RELEASES') -ErrorAction SilentlyContinue
$archive = Join-Path $output "zflow-v$version-windows-x86_64.zip"
Compress-Archive -Path (Join-Path $repo 'windows/dist/*') -DestinationPath $archive -Force
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText("$archive.sha256", "$hash  $([IO.Path]::GetFileName($archive))`n", [Text.UTF8Encoding]::new($false))
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-windows.ps1') -Destination $output
Write-Host "Packaged $archive"
