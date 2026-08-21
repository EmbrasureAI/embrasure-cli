[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$BaseMsi,

    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$UpgradeMsi
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\Embrasure'
$executable = Join-Path $installRoot 'bin\embrasure.exe'
$expectedPath = (Join-Path $installRoot 'bin').TrimEnd('\')
$userPathBefore = [Environment]::GetEnvironmentVariable('Path', 'User')
$configSentinel = Join-Path $env:APPDATA 'embrasure-check\installer-test.txt'
$credentialSentinel = Join-Path $env:APPDATA 'embrasure-check\oauth\credential.bin'
$logRoot = Join-Path $env:RUNNER_TEMP 'embrasure-msi-logs'
$fixtureRoot = Join-Path $env:RUNNER_TEMP 'Embrasure installer fixtures ü'
$exampleConfig = Join-Path $PSScriptRoot '..\..\embrasure-check.example.yml'
New-Item -ItemType Directory -Path (Split-Path -Parent $credentialSentinel), $logRoot, $fixtureRoot -Force | Out-Null
Set-Content -LiteralPath $configSentinel -Value 'preserve me' -Encoding UTF8
[IO.File]::WriteAllBytes($credentialSentinel, [byte[]](1, 2, 3, 4))
$baseFixture = Join-Path $fixtureRoot 'embrasure base.msi'
$upgradeFixture = Join-Path $fixtureRoot 'embrasure upgrade.msi'
Copy-Item -LiteralPath $BaseMsi -Destination $baseFixture
Copy-Item -LiteralPath $UpgradeMsi -Destination $upgradeFixture
$BaseMsi = $baseFixture
$UpgradeMsi = $upgradeFixture
$cleanupMsi = $null

function Invoke-Msi {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('/i', '/x', '/fa')][string]$Action,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$LogName,
        [ValidateSet('/passive', '/qn')][string]$Display = '/qn',
        [switch]$ExpectFailure
    )
    $log = Join-Path $logRoot $LogName
    $arguments = @($Action, ('"' + $Path + '"'), $Display, '/norestart', '/L*v', ('"' + $log + '"'))
    $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\msiexec.exe') `
        -ArgumentList $arguments -Wait -PassThru
    if (-not (Test-Path -LiteralPath $log -PathType Leaf) -or (Get-Item -LiteralPath $log).Length -eq 0) {
        throw "MSI operation did not produce a useful log: ${log}"
    }
    if ($ExpectFailure) {
        if ($process.ExitCode -eq 0) { throw "MSI operation unexpectedly succeeded. Log: ${log}" }
    }
    elseif ($process.ExitCode -ne 0) {
        throw "MSI operation failed with $($process.ExitCode). Log: ${log}"
    }
}

function Assert-Installed {
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw "Installed executable is missing: ${executable}"
    }
    $versionOutput = & $executable --version
    if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^embrasure ') {
        throw "Installed executable failed: ${versionOutput}"
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | ForEach-Object { $_.TrimEnd('\') })
    if (@($entries | Where-Object { $_ -eq $expectedPath }).Count -ne 1) {
        throw 'The installer did not add exactly one Embrasure user PATH entry.'
    }
    $wheel = @(Get-ChildItem -LiteralPath (Join-Path $installRoot 'libexec\embrasure\python') -Filter 'sqlglot-*.whl')
    if ($wheel.Count -ne 1) { throw 'The installed SQLGlot wheel is missing or ambiguous.' }
    $doctorJson = & $executable --config $exampleConfig doctor --read-only --json | Out-String
    if ($LASTEXITCODE -ne 3) { throw "Packaged doctor returned an unexpected exit code: $LASTEXITCODE" }
    $doctor = $doctorJson | ConvertFrom-Json
    $sqlglot = @($doctor.checks | Where-Object { $_.check -eq 'sqlglot' })
    if ($sqlglot.Count -ne 1 -or $sqlglot[0].status -ne 'pass' -or $sqlglot[0].message -ne 'SQLGlot 30.7.0') {
        throw "Packaged SQLGlot probe failed: $($sqlglot | ConvertTo-Json -Compress)"
    }
}

function Assert-InstalledVersion {
    param([Parameter(Mandatory = $true)][string]$Version)

    $roots = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    $entries = @(
        foreach ($root in $roots) {
            Get-ItemProperty $root -ErrorAction SilentlyContinue |
                Where-Object {
                    $_.PSObject.Properties.Name -contains 'DisplayName' -and
                    $_.DisplayName -eq 'Embrasure'
                }
        }
    )
    $versions = @($entries | ForEach-Object { $_.DisplayVersion })
    if ($entries.Count -ne 1 -or $versions[0] -ne $Version) {
        throw "Apps & Features reports an unexpected Embrasure version: $($versions -join ', ')"
    }
    $entry = $entries[0]
    if ($entry.Publisher -ne 'Embrasure, Inc.' -or
        $entry.HelpLink -ne 'https://github.com/EmbrasureAI/embrasure-cli/issues' -or
        $entry.URLInfoAbout -ne 'https://github.com/EmbrasureAI/embrasure-cli' -or
        [string]::IsNullOrWhiteSpace([string]$entry.UninstallString)) {
        throw 'Apps & Features metadata is incomplete or incorrect.'
    }
}

function Assert-NewPowerShellFindsEmbrasure {
    $probe = @'
$env:Path = @(
    [Environment]::GetEnvironmentVariable('Path', 'Machine'),
    [Environment]::GetEnvironmentVariable('Path', 'User')
) -join ';'
$command = Get-Command embrasure.exe -ErrorAction Stop
& $command.Source --version | Out-Null
if ($LASTEXITCODE -ne 0) { exit 1 }
'@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($probe))
    $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $process = Start-Process -FilePath $powershell `
        -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded) `
        -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw 'A new PowerShell process could not run embrasure from the installed user PATH.'
    }
}

function Assert-UserPathRestored {
    param([AllowEmptyString()][string]$Expected)

    $actual = [Environment]::GetEnvironmentVariable('Path', 'User')
    $beforeWithoutTrailingSeparators = ([string]$Expected).TrimEnd(';')
    $afterWithoutTrailingSeparators = ([string]$actual).TrimEnd(';')
    if ($beforeWithoutTrailingSeparators -cne $afterWithoutTrailingSeparators) {
        throw "Uninstall changed a neighboring user PATH entry. Before=<${Expected}> After=<${actual}>"
    }
}

try {
    Invoke-Msi -Action '/i' -Path $BaseMsi -LogName 'install-passive.log' -Display '/passive'
    $cleanupMsi = $BaseMsi
    Assert-Installed
    Assert-NewPowerShellFindsEmbrasure
    Invoke-Msi -Action '/x' -Path $BaseMsi -LogName 'uninstall-passive.log'
    $cleanupMsi = $null
    if (Test-Path -LiteralPath $executable) { throw 'Passive-install uninstall left the executable behind.' }
    Assert-UserPathRestored -Expected $userPathBefore

    Invoke-Msi -Action '/i' -Path $BaseMsi -LogName 'install-quiet.log'
    $cleanupMsi = $BaseMsi
    Assert-Installed
    Assert-InstalledVersion -Version '1.0.0'
    $corruptMsi = Join-Path $fixtureRoot 'embrasure corrupt.msi'
    Copy-Item -LiteralPath $UpgradeMsi -Destination $corruptMsi
    $bytes = [IO.File]::ReadAllBytes($corruptMsi)
    $bytes[0] = $bytes[0] -bxor 0xff
    [IO.File]::WriteAllBytes($corruptMsi, $bytes)
    Invoke-Msi -Action '/i' -Path $corruptMsi -LogName 'corrupt-package.log' -ExpectFailure
    Assert-Installed

    # Replacing an installed file with a directory makes InstallFiles fail after the
    # major-upgrade transaction begins, so Windows Installer must restore v1.0.0.
    $rollbackBlocker = Join-Path $installRoot 'docs\reference\enterprise.md'
    Remove-Item -LiteralPath $rollbackBlocker -Force
    New-Item -ItemType Directory -Path $rollbackBlocker | Out-Null
    Invoke-Msi -Action '/i' -Path $UpgradeMsi -LogName 'upgrade-rollback.log' -ExpectFailure
    Assert-Installed
    Assert-InstalledVersion -Version '1.0.0'
    Remove-Item -LiteralPath $rollbackBlocker -Force

    Invoke-Msi -Action '/i' -Path $UpgradeMsi -LogName 'upgrade.log'
    $cleanupMsi = $UpgradeMsi
    Assert-Installed
    Assert-InstalledVersion -Version '1.0.1'
    Invoke-Msi -Action '/i' -Path $BaseMsi -LogName 'downgrade.log' -ExpectFailure
    Assert-Installed
    Assert-InstalledVersion -Version '1.0.1'
    Invoke-Msi -Action '/fa' -Path $UpgradeMsi -LogName 'repair.log'
    Assert-Installed
    Assert-InstalledVersion -Version '1.0.1'
    Invoke-Msi -Action '/x' -Path $UpgradeMsi -LogName 'uninstall.log'
    $cleanupMsi = $null

    if (Test-Path -LiteralPath $executable) { throw 'Uninstall left the executable behind.' }
    if (-not (Test-Path -LiteralPath $configSentinel)) { throw 'Uninstall removed user configuration.' }
    if (-not (Test-Path -LiteralPath $credentialSentinel)) { throw 'Uninstall removed cached credentials.' }
    Assert-UserPathRestored -Expected $userPathBefore
}
finally {
    if ((Test-Path -LiteralPath $executable) -and $null -ne $cleanupMsi) {
        Invoke-Msi -Action '/x' -Path $cleanupMsi -LogName 'cleanup.log'
    }
}
