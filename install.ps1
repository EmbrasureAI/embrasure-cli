[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$Version = 'latest',

    [switch]$Quiet,

    [ValidateNotNullOrEmpty()]
    [string]$LogPath = (Join-Path $env:LOCALAPPDATA 'Embrasure\logs\installer.log')
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$repository = 'EmbrasureAI/embrasure-cli'
$expectedPublisher = 'Embrasure, Inc.'
$expectedRootThumbprint = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'

function Assert-EmbrasureSignaturePolicy {
    param(
        [Parameter(Mandatory = $true)]
        [System.Management.Automation.SignatureStatus]$Status,
        [AllowEmptyString()]
        [string]$Publisher,
        [AllowEmptyString()]
        [string]$RootThumbprint,
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if ($Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Authenticode signature is not valid for ${Path}: ${Status}"
    }
    if ($Publisher -ne $expectedPublisher) {
        throw "Unexpected Authenticode publisher for ${Path}: ${Publisher}"
    }
    if ($RootThumbprint -ne $expectedRootThumbprint) {
        throw "Unexpected Authenticode trust root for ${Path}: ${RootThumbprint}"
    }
}

function Get-SignatureRootThumbprint {
    param(
        [Parameter(Mandatory = $true)]
        [System.Security.Cryptography.X509Certificates.X509Certificate2]$Certificate
    )

    $chain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
    try {
        # Get-AuthenticodeSignature already validates the timestamped signature. Ignore only
        # leaf expiration here so Artifact Signing's 72-hour certificates remain inspectable.
        $chain.ChainPolicy.VerificationFlags = `
            [System.Security.Cryptography.X509Certificates.X509VerificationFlags]::IgnoreNotTimeValid
        [void]$chain.Build($Certificate)
        if ($chain.ChainElements.Count -eq 0) { return '' }
        return $chain.ChainElements[$chain.ChainElements.Count - 1].Certificate.Thumbprint
    }
    finally {
        $chain.Dispose()
    }
}

function Assert-EmbrasureSignature {
    param([Parameter(Mandatory = $true)][string]$Path)

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    $publisher = ''
    $rootThumbprint = ''
    if ($null -ne $signature.SignerCertificate) {
        $publisher = $signature.SignerCertificate.GetNameInfo(
            [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
            $false
        )
        $rootThumbprint = Get-SignatureRootThumbprint -Certificate $signature.SignerCertificate
    }
    Assert-EmbrasureSignaturePolicy `
        -Status $signature.Status `
        -Publisher $publisher `
        -RootThumbprint $rootThumbprint `
        -Path $Path
}

function Assert-EmbrasureHash {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidatePattern('^[0-9A-Fa-f]{64}$')][string]$ExpectedSha256
    )

    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($actualHash -ne $ExpectedSha256) {
        throw "Checksum verification failed for $(Split-Path -Leaf $Path)."
    }
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

function Quote-NativeArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    if ($Value.Contains('"')) {
        throw 'Installer paths must not contain quotation marks.'
    }
    return '"' + $Value + '"'
}

function Get-SystemMsiExec {
    if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) {
        return Join-Path $env:SystemRoot 'Sysnative\msiexec.exe'
    }
    return Join-Path $env:SystemRoot 'System32\msiexec.exe'
}

function Get-WindowsInstallerFailureMessage {
    param(
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)][string]$LogPath
    )

    if ($ExitCode -eq 1625) {
        return "Windows Installer blocked this current-user MSI with system policy (exit code 1625). On Windows Server, an administrator must allow unmanaged MSI installs by setting 'Turn off Windows Installer' to 'Never' (DisableMSI=0). Log: ${LogPath}"
    }
    return "Windows Installer failed with exit code ${ExitCode}. Log: ${LogPath}"
}

function Invoke-EmbrasureInstall {
    Assert-EmbrasureSignature -Path $PSCommandPath
    $resolvedVersion = Resolve-EmbrasureVersion -RequestedVersion $Version
    $resolvedLogPath = [IO.Path]::GetFullPath($LogPath)
    $target = 'x86_64-pc-windows-msvc'
    $packageName = "embrasure-${resolvedVersion}-${target}.msi"
    $baseUrl = "https://github.com/${repository}/releases/download/v${resolvedVersion}"
    $temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("embrasure-install-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $temporary | Out-Null

    try {
        $headers = @{ 'User-Agent' = 'embrasure-installer' }
        $packagePath = Join-Path $temporary $packageName
        $checksumPath = Join-Path $temporary 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "${baseUrl}/${packageName}" -OutFile $packagePath
        Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "${baseUrl}/SHA256SUMS" -OutFile $checksumPath

        $expectedHash = Read-ExpectedChecksum -ChecksumPath $checksumPath -FileName $packageName
        Assert-EmbrasureHash -Path $packagePath -ExpectedSha256 $expectedHash
        Assert-EmbrasureSignature -Path $packagePath

        $logDirectory = Split-Path -Parent $resolvedLogPath
        New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
        $display = if ($Quiet) { '/qn' } else { '/passive' }
        $msiexec = Get-SystemMsiExec
        $arguments = @(
            '/i',
            (Quote-NativeArgument -Value $packagePath),
            $display,
            '/norestart',
            '/L*v',
            (Quote-NativeArgument -Value $resolvedLogPath)
        )
        $installer = Start-Process -FilePath $msiexec -ArgumentList $arguments -Wait -PassThru
        switch ($installer.ExitCode) {
            0 {
                Write-Host "Installed Embrasure ${resolvedVersion}. Open a new terminal, then run 'embrasure doctor'."
                return
            }
            3010 {
                Write-Warning 'Embrasure was installed, but Windows requires a restart. No restart was forced.'
                $host.SetShouldExit(3010)
                return
            }
            default {
                throw (Get-WindowsInstallerFailureMessage -ExitCode $installer.ExitCode -LogPath $resolvedLogPath)
            }
        }
    }
    finally {
        Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-EmbrasureInstall
}
