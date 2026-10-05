[CmdletBinding()]
param([string]$SourceDirectory, [switch]$NoLaunch)
$ErrorActionPreference = 'Stop'
$assetName = 'zflow-windows-x86_64-Setup.exe'
$temporary = $null
try {
    if ($SourceDirectory) {
        $setup = Join-Path (Resolve-Path -LiteralPath $SourceDirectory).Path $assetName
        if (-not (Test-Path -LiteralPath $setup)) { throw "The installer is missing: $setup. Run scripts/package-windows.ps1 first, or omit -SourceDirectory to download the latest release." }
    } else {
        $temporary = Join-Path ([IO.Path]::GetTempPath()) ('zflow-install-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temporary | Out-Null
        $release = Invoke-RestMethod 'https://api.github.com/repos/demfabris/zflow/releases/latest' -Headers @{ 'User-Agent' = 'zflow-installer' }
        $installerAsset = @($release.assets | Where-Object { $_.name -eq $assetName })
        $checksumsAsset = @($release.assets | Where-Object { $_.name -eq 'SHA256SUMS' })
        if ($installerAsset.Count -ne 1 -or $checksumsAsset.Count -ne 1) { throw 'The latest release has no complete Windows installer. Try again after the release finishes.' }
        # Resolve both files from one release so publication cannot mix versions.
        $setup = Join-Path $temporary $assetName
        Invoke-WebRequest -UseBasicParsing -Uri $installerAsset[0].browser_download_url -OutFile $setup
        $checksumsPath = Join-Path $temporary 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing -Uri $checksumsAsset[0].browser_download_url -OutFile $checksumsPath
        $checksumLines = @(Get-Content -LiteralPath $checksumsPath | Where-Object { $_ -match ('^[a-fA-F0-9]{64}  ' + [regex]::Escape($assetName) + '$') })
        if ($checksumLines.Count -ne 1) { throw 'The release checksum for the Windows installer is missing or ambiguous' }
        $expected = $checksumLines[0].Substring(0, 64)
        if ((Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash -ne $expected) { throw 'The Windows installer checksum does not match the release' }
    }
    $installed = Join-Path $env:LOCALAPPDATA 'Zflow.App/Zflow.App.exe'
    $legacy = Join-Path $env:LOCALAPPDATA 'Programs/zflow/Zflow.App.exe'
    foreach ($app in @($installed, $legacy)) {
        if (Test-Path -LiteralPath $app) { Start-Process -FilePath $app -ArgumentList '--quit' -Wait }
    }
    $directories = @((Split-Path -Parent $installed), (Split-Path -Parent $legacy))
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    do {
        $running = @(Get-CimInstance Win32_Process -Filter "Name = 'Zflow.App.exe' OR Name = 'zflow.exe'" | Where-Object {
            $path = $_.ExecutablePath
            $path -and ($directories | Where-Object { $path.StartsWith($_ + '\', [StringComparison]::OrdinalIgnoreCase) })
        })
        if (-not $running) { break }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($running) { throw 'Quit zflow and its input engine before installing. If zflow runs as administrator, quit it from its notification-area menu.' }
    $result = Start-Process -FilePath $setup -ArgumentList '--silent' -Wait -PassThru
    if ($result.ExitCode -ne 0) { throw "The Windows installer failed (exit code $($result.ExitCode))" }
    if (-not (Test-Path -LiteralPath $installed)) { throw 'The installer did not create the zflow launcher' }
    Write-Host "Installed zflow for this user at $(Split-Path -Parent $installed)"
    Write-Host 'Your configuration and paired computers remain in %LOCALAPPDATA%\zflow.'
    if (-not $NoLaunch) { Start-Process -FilePath $installed }
} finally {
    if ($temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
