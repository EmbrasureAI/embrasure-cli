[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$Version = 'latest',

    [ValidateNotNullOrEmpty()]
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Embrasure'),

    [switch]$Quiet,

    [ValidateNotNullOrEmpty()]
    [string]$LogPath = (Join-Path $env:LOCALAPPDATA 'Embrasure\logs\installer.log'),

    [switch]$NoPath,

    [switch]$Uninstall,

    [Parameter(DontShow = $true)]
    [string]$ArchivePath,

    [Parameter(DontShow = $true)]
    [string]$ExpectedSha256,

    [Parameter(DontShow = $true)]
    [ValidateRange(0, [int]::MaxValue)]
    [int]$WaitForPid = 0,

    [Parameter(DontShow = $true)]
    [switch]$CleanupArchiveDirectory
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repository = 'EmbrasureAI/embrasure-cli'
$target = 'x86_64-pc-windows-msvc'
$maximumArchiveBytes = 268435456
$maximumArchiveEntries = 4096

function Write-InstallerLog {
    param([Parameter(Mandatory = $true)][string]$Message)

    $resolvedLogPath = [IO.Path]::GetFullPath($LogPath)
    $logDirectory = Split-Path -Parent $resolvedLogPath
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    $timestamp = [DateTime]::UtcNow.ToString('o')
    Add-Content -LiteralPath $resolvedLogPath -Value "${timestamp} ${Message}" -Encoding UTF8
}

function Resolve-EmbrasureVersion {
    param([Parameter(Mandatory = $true)][string]$RequestedVersion)

    if ($RequestedVersion -eq 'latest') {
        $headers = @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'embrasure-installer' }
        $release = Invoke-RestMethod `
            -Uri "https://api.github.com/repos/${repository}/releases/latest" `
            -Headers $headers
        $tag = [string]$release.tag_name
        if ($tag -notmatch '^v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))$') {
            throw "Latest GitHub release has an invalid version tag: ${tag}"
        }
        return $Matches[1]
    }
    if ($RequestedVersion -notmatch '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$') {
        throw "Invalid Embrasure version: ${RequestedVersion}"
    }
    return $RequestedVersion
}

function Read-ExpectedChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$ChecksumPath,
        [Parameter(Mandatory = $true)][string]$FileName
    )

    $found = @()
    foreach ($line in Get-Content -LiteralPath $ChecksumPath) {
        if ($line -match '^(\S+)[\t ]+\*?(.+)$' -and $Matches[2] -eq $FileName) {
            $found += $Matches[1]
        }
    }
    if ($found.Count -ne 1) {
        throw "SHA256SUMS must contain exactly one entry for ${FileName}; found $($found.Count)."
    }
    if ($found[0] -notmatch '^[0-9A-Fa-f]{64}$') {
        throw "SHA256SUMS contains an invalid SHA-256 value for ${FileName}."
    }
    return $found[0]
}

function Assert-EmbrasureHash {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidatePattern('^[0-9A-Fa-f]{64}$')][string]$Expected
    )

    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($actual -ne $Expected) {
        throw "Checksum verification failed for $(Split-Path -Leaf $Path)."
    }
}

function Get-SafeInstallDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $resolved = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    $root = [IO.Path]::GetPathRoot($resolved).TrimEnd('\', '/')
    if ([string]::IsNullOrWhiteSpace($resolved) -or $resolved -eq $root) {
        throw "Refusing unsafe installation directory: ${Path}"
    }
    if (Test-Path -LiteralPath $resolved) {
        $item = Get-Item -LiteralPath $resolved -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing installation through a reparse point: ${resolved}"
        }
    }
    return $resolved
}

function Assert-SafeZipArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedRoot
    )

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        if ($archive.Entries.Count -gt $maximumArchiveEntries) {
            throw "Archive contains too many entries: $($archive.Entries.Count)."
        }
        $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        [long]$totalBytes = 0
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            $parts = @($name.Split('/') | Where-Object { $_ -ne '' })
            if ($parts.Count -eq 0 -or
                $parts[0] -ne $ExpectedRoot -or
                $name.StartsWith('/') -or
                $name -match '^[A-Za-z]:' -or
                $name.Contains(':') -or
                $parts -contains '.' -or
                $parts -contains '..') {
                throw "Archive contains an unsafe path: $($entry.FullName)"
            }
            if (-not $seen.Add($name)) {
                throw "Archive contains a duplicate path: $($entry.FullName)"
            }
            $unixFileType = (($entry.ExternalAttributes -shr 16) -band 0xF000)
            if ($unixFileType -eq 0xA000) {
                throw "Archive contains an unsupported symbolic link: $($entry.FullName)"
            }
            $totalBytes += $entry.Length
            if ($totalBytes -gt $maximumArchiveBytes) {
                throw 'Archive expands beyond the 256 MiB safety limit.'
            }
        }
    }
    finally {
        $archive.Dispose()
    }
}

function Test-EmbrasurePayload {
    param(
        [Parameter(Mandatory = $true)][string]$PayloadRoot,
        [Parameter(Mandatory = $true)][string]$ExpectedVersion
    )

    $binary = Join-Path $PayloadRoot 'bin\embrasure.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'Windows archive is missing bin\embrasure.exe.'
    }
    $wheels = @(Get-ChildItem `
        -LiteralPath (Join-Path $PayloadRoot 'libexec\embrasure\python') `
        -Filter 'sqlglot-*.whl' `
        -File `
        -ErrorAction SilentlyContinue)
    if ($wheels.Count -ne 1 -or $wheels[0].Name -ne 'sqlglot-30.7.0-py3-none-any.whl') {
        throw 'Windows archive must contain exactly the pinned SQLGlot 30.7.0 wheel.'
    }
    $reportedVersion = (& $binary --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reportedVersion -ne "embrasure ${ExpectedVersion}") {
        throw "Windows archive reports an unexpected version: ${reportedVersion}"
    }
}

function Get-PathEntries {
    param([AllowEmptyString()][string]$Value)
    if ([string]::IsNullOrEmpty($Value)) { return @() }
    return @($Value -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
}

function Test-SamePath {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )
    try {
        return [IO.Path]::GetFullPath($Left).TrimEnd('\', '/') -ieq `
            [IO.Path]::GetFullPath($Right).TrimEnd('\', '/')
    }
    catch {
        return $Left.TrimEnd('\', '/') -ieq $Right.TrimEnd('\', '/')
    }
}

function Add-EmbrasureToUserPath {
    param([Parameter(Mandatory = $true)][string]$BinDirectory)

    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @(Get-PathEntries -Value $current)
    if (-not ($entries | Where-Object { Test-SamePath -Left $_ -Right $BinDirectory })) {
        $entries += $BinDirectory
        [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
    }
    if (-not (Get-PathEntries -Value $env:Path | Where-Object { Test-SamePath -Left $_ -Right $BinDirectory })) {
        $env:Path = if ([string]::IsNullOrEmpty($env:Path)) { $BinDirectory } else { "${env:Path};${BinDirectory}" }
    }
}

function Remove-EmbrasureFromUserPath {
    param([Parameter(Mandatory = $true)][string]$BinDirectory)

    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @(Get-PathEntries -Value $current | Where-Object {
        -not (Test-SamePath -Left $_ -Right $BinDirectory)
    })
    [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
}

function Install-EmbrasureArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$ArchiveVersion,
        [switch]$SkipPath,
        [int]$ParentPid = 0
    )

    $safeDestination = Get-SafeInstallDirectory -Path $Destination
    $parentDirectory = Split-Path -Parent $safeDestination
    New-Item -ItemType Directory -Path $parentDirectory -Force | Out-Null
    $packageRootName = "embrasure-${ArchiveVersion}-${target}"
    Assert-SafeZipArchive -Path $Path -ExpectedRoot $packageRootName

    $identifier = [guid]::NewGuid().ToString('N')
    $extractionDirectory = Join-Path $parentDirectory ".embrasure-extract-${identifier}"
    $backupDirectory = Join-Path $parentDirectory ".embrasure-backup-${identifier}"
    $installedNewPayload = $false
    $movedOldPayload = $false
    try {
        Expand-Archive -LiteralPath $Path -DestinationPath $extractionDirectory
        $payloadRoot = Join-Path $extractionDirectory $packageRootName
        Test-EmbrasurePayload -PayloadRoot $payloadRoot -ExpectedVersion $ArchiveVersion

        if ($ParentPid -gt 0) {
            $parent = Get-Process -Id $ParentPid -ErrorAction SilentlyContinue
            if ($null -ne $parent) { $parent.WaitForExit() }
        }

        if (Test-Path -LiteralPath $safeDestination) {
            Move-Item -LiteralPath $safeDestination -Destination $backupDirectory
            $movedOldPayload = $true
        }
        Move-Item -LiteralPath $payloadRoot -Destination $safeDestination
        $installedNewPayload = $true

        if (-not $SkipPath) {
            Add-EmbrasureToUserPath -BinDirectory (Join-Path $safeDestination 'bin')
        }
        if ($movedOldPayload) {
            Remove-Item -LiteralPath $backupDirectory -Recurse -Force
            $movedOldPayload = $false
        }
    }
    catch {
        $installFailure = $_
        if ($installedNewPayload -and (Test-Path -LiteralPath $safeDestination)) {
            Remove-Item -LiteralPath $safeDestination -Recurse -Force -ErrorAction SilentlyContinue
        }
        if ($movedOldPayload -and (Test-Path -LiteralPath $backupDirectory)) {
            try {
                Move-Item -LiteralPath $backupDirectory -Destination $safeDestination
                $movedOldPayload = $false
            }
            catch {
                throw "Installation failed and rollback could not restore ${safeDestination}: $($_.Exception.Message). Backup: ${backupDirectory}"
            }
        }
        throw $installFailure
    }
    finally {
        Remove-Item -LiteralPath $extractionDirectory -Recurse -Force -ErrorAction SilentlyContinue
        if (-not $movedOldPayload) {
            Remove-Item -LiteralPath $backupDirectory -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Uninstall-Embrasure {
    param(
        [Parameter(Mandatory = $true)][string]$Destination,
        [switch]$KeepPath
    )

    $safeDestination = Get-SafeInstallDirectory -Path $Destination
    if (Test-Path -LiteralPath $safeDestination) {
        Remove-Item -LiteralPath $safeDestination -Recurse -Force
    }
    if (-not $KeepPath) {
        Remove-EmbrasureFromUserPath -BinDirectory (Join-Path $safeDestination 'bin')
    }
}

function Invoke-EmbrasureInstall {
    $safeInstallDir = Get-SafeInstallDirectory -Path $InstallDir
    if ($Uninstall) {
        Write-InstallerLog "Uninstalling from ${safeInstallDir}."
        Uninstall-Embrasure -Destination $safeInstallDir -KeepPath:$NoPath
        Write-InstallerLog 'Uninstall completed.'
        if (-not $Quiet) { Write-Host 'Embrasure was uninstalled. User configuration and credentials were preserved.' }
        return
    }

    $resolvedVersion = Resolve-EmbrasureVersion -RequestedVersion $Version
    $packageName = "embrasure-${resolvedVersion}-${target}.zip"
    $temporary = $null
    $packagePath = $ArchivePath
    try {
        if ([string]::IsNullOrEmpty($packagePath)) {
            $temporary = Join-Path ([IO.Path]::GetTempPath()) ("embrasure-install-" + [guid]::NewGuid())
            New-Item -ItemType Directory -Path $temporary | Out-Null
            $packagePath = Join-Path $temporary $packageName
            $checksumPath = Join-Path $temporary 'SHA256SUMS'
            $baseUrl = "https://github.com/${repository}/releases/download/v${resolvedVersion}"
            $headers = @{ 'User-Agent' = 'embrasure-installer' }
            Write-InstallerLog "Downloading ${packageName} from ${baseUrl}."
            if (-not $Quiet) { Write-Host "Downloading Embrasure ${resolvedVersion}..." }
            Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "${baseUrl}/${packageName}" -OutFile $packagePath
            Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "${baseUrl}/SHA256SUMS" -OutFile $checksumPath
            $ExpectedSha256 = Read-ExpectedChecksum -ChecksumPath $checksumPath -FileName $packageName
        }
        elseif ($ExpectedSha256 -notmatch '^[0-9A-Fa-f]{64}$') {
            throw 'A valid expected SHA-256 is required with an existing archive.'
        }

        Assert-EmbrasureHash -Path $packagePath -Expected $ExpectedSha256
        Write-InstallerLog "Checksum verified for ${packageName}."
        Install-EmbrasureArchive `
            -Path $packagePath `
            -Destination $safeInstallDir `
            -ArchiveVersion $resolvedVersion `
            -SkipPath:$NoPath `
            -ParentPid $WaitForPid
        Write-InstallerLog "Installed Embrasure ${resolvedVersion} to ${safeInstallDir}."
        if (-not $Quiet) {
            Write-Host "Installed Embrasure ${resolvedVersion}. Open a new terminal, then run 'embrasure doctor'."
        }
    }
    catch {
        Write-InstallerLog "Installation failed: $($_.Exception.Message)"
        throw
    }
    finally {
        if ($null -ne $temporary) {
            Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
        }
        if ($CleanupArchiveDirectory -and -not [string]::IsNullOrEmpty($ArchivePath)) {
            $archiveDirectory = Split-Path -Parent ([IO.Path]::GetFullPath($ArchivePath))
            Remove-Item -LiteralPath $archiveDirectory -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-EmbrasureInstall
}
