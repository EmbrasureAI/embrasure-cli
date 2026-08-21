[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$ArchivePath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$')]
    [string]$Version,

    [ValidateSet('WindowsPowerShell', 'PowerShell')]
    [string]$InstallerEngine = 'WindowsPowerShell'
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

$installerScript = (Resolve-Path (Join-Path $PSScriptRoot '..\..\install.ps1')).Path
$testRoot = Join-Path $env:RUNNER_TEMP 'Embrasure ZIP lifecycle ü'
$installRoot = Join-Path $testRoot 'Programs with spaces\Embrasure'
$configRoot = Join-Path $testRoot 'preserved-user-data'
$configFile = Join-Path $configRoot 'config.yml'
$logPath = Join-Path $testRoot 'logs\installer.log'
$archiveHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash
$originalUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$neighborBefore = Join-Path $testRoot 'neighbor-before'
$neighborAfter = Join-Path $testRoot 'neighbor-after'
$installerPowerShell = if ($InstallerEngine -eq 'PowerShell') {
    (Get-Command pwsh.exe -ErrorAction Stop).Source
} else {
    Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
}

function Invoke-InstallerProcess {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)

    & $installerPowerShell @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Installer process failed with exit code ${LASTEXITCODE}."
    }
}

if (Test-Path -LiteralPath $testRoot) {
    Remove-Item -LiteralPath $testRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $configRoot, $neighborBefore, $neighborAfter -Force | Out-Null
Set-Content -LiteralPath $configFile -Value 'preserve: true' -Encoding UTF8

try {
    [Environment]::SetEnvironmentVariable(
        'Path',
        "${neighborBefore};${neighborAfter}",
        'User'
    )

    $installArguments = @(
        '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
        '-File', $installerScript,
        '-Version', $Version,
        '-InstallDir', $installRoot,
        '-ArchivePath', (Resolve-Path $ArchivePath).Path,
        '-ExpectedSha256', $archiveHash,
        '-LogPath', $logPath,
        '-Quiet'
    )
    Invoke-InstallerProcess -Arguments $installArguments

    $binary = Join-Path $installRoot 'bin\embrasure.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'Installed executable is missing.'
    }
    $reportedVersion = (& $binary --version | Out-String).Trim()
    if ($reportedVersion -ne "embrasure ${Version}") {
        throw "Installed executable reports an unexpected version: ${reportedVersion}"
    }
    $wheels = @(Get-ChildItem `
        -LiteralPath (Join-Path $installRoot 'libexec\embrasure\python') `
        -Filter 'sqlglot-*.whl' `
        -File)
    if ($wheels.Count -ne 1 -or $wheels[0].Name -ne 'sqlglot-30.7.0-py3-none-any.whl') {
        throw 'Installed SQLGlot inventory is incorrect.'
    }
    $exampleConfig = Join-Path $installRoot 'docs\embrasure-check.example.yml'
    $doctorJson = & $binary --config $exampleConfig doctor --read-only --json | Out-String
    if ($LASTEXITCODE -ne 3) {
        throw "Packaged doctor returned an unexpected exit code: ${LASTEXITCODE}"
    }
    $doctor = $doctorJson | ConvertFrom-Json
    $sqlglot = @($doctor.checks | Where-Object { $_.check -eq 'sqlglot' })
    if ($sqlglot.Count -ne 1 -or
        $sqlglot[0].status -ne 'pass' -or
        $sqlglot[0].message -ne 'SQLGlot 30.7.0') {
        throw "Packaged SQLGlot probe failed: $($sqlglot | ConvertTo-Json -Compress)"
    }
    $global:LASTEXITCODE = 0
    if (-not (Test-Path -LiteralPath $logPath -PathType Leaf) -or
        -not (Select-String -LiteralPath $logPath -SimpleMatch 'Checksum verified' -Quiet)) {
        throw 'Installer did not retain a useful log.'
    }

    $installedBin = Join-Path $installRoot 'bin'
    $pathEntries = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';')
    if (@($pathEntries | Where-Object { $_ -ieq $installedBin }).Count -ne 1 -or
        -not ($pathEntries -contains $neighborBefore) -or
        -not ($pathEntries -contains $neighborAfter)) {
        throw 'Installer did not add exactly one owned PATH entry while preserving neighbors.'
    }

    $childPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $savedProcessPath = $env:Path
    try {
        $env:Path = $childPath
        $probe = 'embrasure.exe --version | Out-Null; if ($LASTEXITCODE -ne 0) { exit 1 }'
        & $installerPowerShell -NoLogo -NoProfile -NonInteractive -Command $probe
        if ($LASTEXITCODE -ne 0) { throw 'A new PowerShell process could not run embrasure from PATH.' }
    }
    finally {
        $env:Path = $savedProcessPath
    }

    $staleFile = Join-Path $installRoot 'stale-upgrade-file.txt'
    Set-Content -LiteralPath $staleFile -Value 'must be removed' -Encoding ASCII
    Invoke-InstallerProcess -Arguments $installArguments
    if (Test-Path -LiteralPath $staleFile) {
        throw 'Same-version reinstall did not replace the old payload.'
    }
    $pathEntries = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';')
    if (@($pathEntries | Where-Object { $_ -ieq $installedBin }).Count -ne 1) {
        throw 'Same-version reinstall duplicated the PATH entry.'
    }

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $invalidArchive = Join-Path $testRoot 'invalid-layout.zip'
    $zip = [IO.Compression.ZipFile]::Open($invalidArchive, [IO.Compression.ZipArchiveMode]::Create)
    try {
        [void]$zip.CreateEntry("embrasure-${Version}-x86_64-pc-windows-msvc/docs/missing-binary.txt")
    }
    finally {
        $zip.Dispose()
    }
    . $installerScript
    try {
        Install-EmbrasureArchive `
            -Path $invalidArchive `
            -Destination $installRoot `
            -ArchiveVersion $Version `
            -SkipPath
        throw 'Invalid upgrade archive was accepted.'
    }
    catch {
        if ($_.Exception.Message -eq 'Invalid upgrade archive was accepted.') { throw }
    }
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'Failed upgrade did not preserve the installed payload.'
    }

    $uninstallArguments = @(
        '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
        '-File', $installerScript,
        '-InstallDir', $installRoot,
        '-LogPath', $logPath,
        '-Uninstall', '-Quiet'
    )
    Invoke-InstallerProcess -Arguments $uninstallArguments
    if (Test-Path -LiteralPath $installRoot) {
        throw 'Uninstall left the application payload behind.'
    }
    $remainingPath = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';')
    if ($remainingPath | Where-Object { $_ -ieq $installedBin }) {
        throw 'Uninstall left its owned PATH entry behind.'
    }
    if (-not ($remainingPath -contains $neighborBefore) -or
        -not ($remainingPath -contains $neighborAfter)) {
        throw 'Uninstall damaged neighboring PATH entries.'
    }
    if (-not (Test-Path -LiteralPath $configFile -PathType Leaf)) {
        throw 'Uninstall removed user data.'
    }
}
finally {
    [Environment]::SetEnvironmentVariable('Path', $originalUserPath, 'User')
    Remove-Item -LiteralPath $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
