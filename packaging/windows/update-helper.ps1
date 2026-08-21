[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$ParentPid,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$MsiPath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9A-Fa-f]{64}$')]
    [string]$ExpectedSha256,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$LogPath
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$expectedPublisher = 'Embrasure, Inc.'
$expectedRootThumbprint = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'

function Get-SignatureRootThumbprint {
    param(
        [Parameter(Mandatory = $true)]
        [System.Security.Cryptography.X509Certificates.X509Certificate2]$Certificate
    )

    $chain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
    try {
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
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Authenticode signature is not valid for ${Path}: $($signature.Status)"
    }
    $publisher = $signature.SignerCertificate.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
        $false
    )
    if ($publisher -ne $expectedPublisher) {
        throw "Unexpected Authenticode publisher for ${Path}: ${publisher}"
    }
    $rootThumbprint = Get-SignatureRootThumbprint -Certificate $signature.SignerCertificate
    if ($rootThumbprint -ne $expectedRootThumbprint) {
        throw "Unexpected Authenticode trust root for ${Path}: ${rootThumbprint}"
    }
}

function Quote-NativeArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    if ($Value.Contains('"')) {
        throw 'Installer paths must not contain quotation marks.'
    }
    return '"' + $Value + '"'
}

function Get-WindowsInstallerFailureMessage {
    param(
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)][string]$InstallerLogPath
    )

    if ($ExitCode -eq 1625) {
        return "Windows Installer blocked this current-user MSI with system policy (exit code 1625). On Windows Server, an administrator must allow unmanaged MSI installs by setting 'Turn off Windows Installer' to 'Never' (DisableMSI=0). Log: ${InstallerLogPath}"
    }
    return "Windows Installer failed with exit code ${ExitCode}. Log: ${InstallerLogPath}"
}

try {
    Assert-EmbrasureSignature -Path $PSCommandPath
    $parent = Get-Process -Id $ParentPid -ErrorAction SilentlyContinue
    if ($null -ne $parent) {
        $parent.WaitForExit()
    }

    if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf)) {
        throw "Downloaded installer is missing: ${MsiPath}"
    }
    $actualHash = (Get-FileHash -LiteralPath $MsiPath -Algorithm SHA256).Hash
    if ($actualHash -ne $ExpectedSha256) {
        throw 'Downloaded installer changed after verification.'
    }
    Assert-EmbrasureSignature -Path $MsiPath

    $logDirectory = Split-Path -Parent $LogPath
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
    $msiexec = Join-Path $env:SystemRoot 'System32\msiexec.exe'
    $arguments = @(
        '/i',
        (Quote-NativeArgument -Value $MsiPath),
        '/passive',
        '/norestart',
        '/L*v',
        (Quote-NativeArgument -Value $LogPath)
    )
    $installer = Start-Process -FilePath $msiexec -ArgumentList $arguments -Wait -PassThru
    switch ($installer.ExitCode) {
        0 { Write-Host 'Embrasure was updated successfully.'; exit 0 }
        3010 { Write-Warning 'Embrasure was updated, but Windows requires a restart.'; exit 3010 }
        default { throw (Get-WindowsInstallerFailureMessage -ExitCode $installer.ExitCode -InstallerLogPath $LogPath) }
    }
}
catch {
    Write-Error $_
    exit 1
}
finally {
    if (Test-Path -LiteralPath $MsiPath -PathType Leaf) {
        Remove-Item -LiteralPath $MsiPath -Force -ErrorAction SilentlyContinue
    }
    $downloadDirectory = Split-Path -Parent $MsiPath
    Remove-Item -LiteralPath $downloadDirectory -Force -ErrorAction SilentlyContinue
}
