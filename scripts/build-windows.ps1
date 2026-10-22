[CmdletBinding()]
param([switch]$Launch)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
try {
    $target = 'x86_64-pc-windows-msvc'
    $metadata = & cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
    $built = Join-Path $metadata.target_directory "$target/release"
    # Cargo chooses one flag source. Append to the active environment override,
    # or merge the target flags through Cargo's own configuration support.
    $flagVariable = @('CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS') |
        Where-Object { $null -ne [Environment]::GetEnvironmentVariable($_) } | Select-Object -First 1
    $previousFlags = if ($flagVariable) { [Environment]::GetEnvironmentVariable($flagVariable) } else { $null }
    $cargoConfig = @()
    $configFile = $null
    try {
        if ($flagVariable) {
            $separator = if ($flagVariable -eq 'CARGO_ENCODED_RUSTFLAGS') { [string][char]31 } else { ' ' }
            $flags = @($previousFlags, '-C', 'target-feature=+crt-static') | Where-Object { $_ -ne '' }
            [Environment]::SetEnvironmentVariable($flagVariable, ($flags -join $separator))
        } else {
            # A config file avoids native argument quote differences in Windows PowerShell.
            $configFile = [IO.Path]::GetTempFileName()
            [IO.File]::WriteAllText($configFile, 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]', [Text.UTF8Encoding]::new($false))
            $cargoConfig = @('--config', $configFile)
        }
        # An explicit target keeps these flags off host build scripts and proc macros.
        & cargo build --locked --release --target $target -p zflow-kvm -p zflow-app --bin zflow --bin zflow-app @cargoConfig
        if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
    } finally {
        if ($flagVariable) { [Environment]::SetEnvironmentVariable($flagVariable, $previousFlags) }
        if ($configFile) { Remove-Item -LiteralPath $configFile -Force }
    }
    # Existing installs, Velopack's main executable and the Run key all use the
    # name Zflow.App.exe, so the app keeps it.
    $binaries = [ordered]@{ 'zflow-app.exe' = 'Zflow.App.exe'; 'zflow.exe' = 'zflow.exe' }
    $output = Join-Path $repo 'windows/dist'
    if (Test-Path -LiteralPath $output) { Remove-Item -LiteralPath $output -Recurse -Force }
    New-Item -ItemType Directory -Path $output | Out-Null
    foreach ($name in $binaries.Keys) {
        $path = Join-Path $built $name
        if (-not (Test-Path -LiteralPath $path)) { throw "Build output is missing: $path" }
        Copy-Item -LiteralPath $path -Destination (Join-Path $output $binaries[$name])
    }
    $dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
    if (-not $dumpbin) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $dumpbin = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC/Tools/MSVC/**/bin/Hostx64/x64/dumpbin.exe' |
            Select-Object -First 1
    }
    if (-not $dumpbin) { throw 'Visual Studio dumpbin.exe is required to verify the packaged executables' }
    foreach ($name in $binaries.Values) {
        $packaged = Join-Path $output $name
        $headers = & $dumpbin /HEADERS $packaged
        if ($LASTEXITCODE -ne 0 -or -not ($headers -match '^\s*8664 machine')) { throw "The packaged $name must be Windows x64" }
        $dependencies = & $dumpbin /DEPENDENTS $packaged
        if ($LASTEXITCODE -ne 0) { throw "Could not inspect the dependencies of the packaged $name" }
        if ($dependencies -match '^\s*(VCRUNTIME|MSVCP|CONCRT|VCCORLIB|MSVCR)\d[^\s]*\.dll\s*$') {
            throw "The packaged $name requires a Visual C++ runtime DLL; build it with the static CRT"
        }
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-windows.ps1') -Destination $output
    Copy-Item -LiteralPath (Join-Path $repo 'windows/README.md') -Destination $output
    Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $output
    Write-Host "Built zflow in $output"
    if ($Launch) { Start-Process -FilePath (Join-Path $output 'Zflow.App.exe') }
} finally { Pop-Location }
