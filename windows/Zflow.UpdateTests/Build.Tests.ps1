$ErrorActionPreference = 'Stop'
$buildScript = Join-Path $PSScriptRoot '../../scripts/build-windows.ps1'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('zflow-build-test-' + [Guid]::NewGuid().ToString('N'))
$flagVariables = @('CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')
$previousEnvironment = @{}
foreach ($name in @($flagVariables) + 'LOCALAPPDATA') { $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name) }
$global:ZflowBuildTestState = @{
    root = $testRoot
    flags = @{}
    arguments = @()
    configuration = ''
    architecture = '8664'
    dependencies = @('KERNEL32.dll', 'api-ms-win-crt-runtime-l1-1-0.dll')
    failBuild = $false
}

function Get-Command {
    param($Name, $ErrorAction)
    if ($Name -eq 'dotnet') { return @{ Source = 'Invoke-TestDotnet' } }
    if ($Name -eq 'dumpbin.exe') { return @{ Source = 'Invoke-TestDumpbin' } }
    throw "Unexpected command lookup: $Name"
}
function cargo {
    if ($args[0] -eq 'metadata') {
        $global:LASTEXITCODE = 0
        return @{ target_directory = (Join-Path $global:ZflowBuildTestState.root 'custom target') } | ConvertTo-Json
    }
    $global:ZflowBuildTestState.arguments = $args
    $configIndex = [Array]::IndexOf($args, '--config')
    $global:ZflowBuildTestState.configuration = if ($configIndex -ge 0) { Get-Content -LiteralPath $args[$configIndex + 1] -Raw } else { '' }
    foreach ($name in @('CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) {
        $global:ZflowBuildTestState.flags[$name] = [Environment]::GetEnvironmentVariable($name)
    }
    $global:LASTEXITCODE = [int]$global:ZflowBuildTestState.failBuild
}
function Invoke-TestDotnet {
    $output = $args[[Array]::IndexOf($args, '--output') + 1]
    $engineArgument = @($args | Where-Object { $_ -like '-p:ZflowEnginePath=*' })
    if ($engineArgument.Count -ne 1) { throw 'Publish must receive the exact Cargo target artifact' }
    $engine = $engineArgument[0].Substring('-p:ZflowEnginePath='.Length)
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    Copy-Item -LiteralPath $engine -Destination (Join-Path $output 'zflow.exe')
    $global:LASTEXITCODE = 0
}
function Invoke-TestDumpbin {
    Require ((Get-Content -LiteralPath $args[1] -Raw) -eq 'x64 target engine') 'The published engine must come from the explicit target directory'
    $global:LASTEXITCODE = 0
    if ($args[0] -eq '/HEADERS') { return "            $($global:ZflowBuildTestState.architecture) machine (x64)" }
    return @('  Image has the following dependencies:') + $global:ZflowBuildTestState.dependencies
}
function Require($Condition, $Message) { if (-not $Condition) { throw $Message } }
function ExpectFailure($Pattern, $Action) {
    $failure = $null
    try { & $Action } catch { $failure = $_.Exception.Message }
    Require ($failure -like $Pattern) "Expected '$Pattern', got '$failure'"
}
function ClearFlags {
    foreach ($name in @('CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
}

try {
    foreach ($directory in @('scripts', 'windows', 'custom target/x86_64-pc-windows-msvc/release', 'target/release')) {
        New-Item -ItemType Directory -Path (Join-Path $testRoot $directory) -Force | Out-Null
    }
    Copy-Item -LiteralPath $buildScript -Destination (Join-Path $testRoot 'scripts/build-windows.ps1')
    foreach ($file in @('LICENSE', 'windows/README.md', 'scripts/install-windows.ps1')) {
        [IO.File]::WriteAllText((Join-Path $testRoot $file), 'fixture')
    }
    [IO.File]::WriteAllText((Join-Path $testRoot 'Cargo.toml'), 'version = "1.2.3"')
    [IO.File]::WriteAllText((Join-Path $testRoot 'custom target/x86_64-pc-windows-msvc/release/zflow.exe'), 'x64 target engine')
    [IO.File]::WriteAllText((Join-Path $testRoot 'target/release/zflow.exe'), 'stale host engine')
    $env:LOCALAPPDATA = Join-Path $testRoot 'profile'
    $build = Join-Path $testRoot 'scripts/build-windows.ps1'

    ClearFlags
    & $build
    $arguments = $global:ZflowBuildTestState.arguments
    Require ($arguments[[Array]::IndexOf($arguments, '--target') + 1] -eq 'x86_64-pc-windows-msvc') 'Cargo must build the packaged architecture explicitly'
    Require ($global:ZflowBuildTestState.configuration -eq 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]') 'Without environment overrides, Cargo must merge the target flags'

    foreach ($name in $flagVariables) {
        ClearFlags
        $separator = if ($name -eq 'CARGO_ENCODED_RUSTFLAGS') { [string][char]31 } else { ' ' }
        $original = '-C' + $separator + 'target-feature=-crt-static' + $separator + '--cfg=existing_flag'
        [Environment]::SetEnvironmentVariable($name, $original)
        & $build
        Require ($global:ZflowBuildTestState.flags[$name] -eq "$original${separator}-C${separator}target-feature=+crt-static") 'Existing Rust flags must remain intact, with static CRT taking precedence'
        Require ([Environment]::GetEnvironmentVariable($name) -eq $original) 'Build flags must be restored afterwards'
    }

    $env:CARGO_ENCODED_RUSTFLAGS = '--cfg=highest_priority'
    $env:RUSTFLAGS = '--cfg=lower_priority'
    & $build
    Require ($global:ZflowBuildTestState.flags.CARGO_ENCODED_RUSTFLAGS.EndsWith('target-feature=+crt-static')) 'The highest-priority Cargo flag source must get the static CRT flag'
    Require ($global:ZflowBuildTestState.flags.RUSTFLAGS -eq '--cfg=lower_priority') 'Lower-priority flags must remain unchanged'
    $global:ZflowBuildTestState.failBuild = $true
    ExpectFailure '*Rust engine build failed*' { & $build }
    Require ($env:CARGO_ENCODED_RUSTFLAGS -eq '--cfg=highest_priority') 'Flags must also be restored after a failed build'
    $global:ZflowBuildTestState.failBuild = $false

    ClearFlags
    $global:ZflowBuildTestState.architecture = 'AA64'
    ExpectFailure '*must be Windows x64*' { & $build }
    $global:ZflowBuildTestState.architecture = '8664'
    $global:ZflowBuildTestState.dependencies += '    VCRUNTIME140.dll'
    ExpectFailure '*requires a Visual C++ runtime DLL*' { & $build }
    Write-Output 'Windows build checks passed.'
} finally {
    foreach ($name in $previousEnvironment.Keys) {
        if ($null -eq $previousEnvironment[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name]) }
    }
    Remove-Variable ZflowBuildTestState -Scope Global
    Remove-Item -LiteralPath $testRoot -Recurse -Force
}
