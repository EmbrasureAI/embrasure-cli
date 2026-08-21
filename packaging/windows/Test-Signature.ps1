[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string[]]$Path
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$expectedPublisher = 'Embrasure, Inc.'
$expectedRootThumbprint = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'

foreach ($artifact in $Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $artifact
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or
        $null -eq $signature.SignerCertificate) {
        throw "Invalid Authenticode signature for ${artifact}: $($signature.Status)"
    }

    $publisher = $signature.SignerCertificate.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
        $false
    )
    if ($publisher -ne $expectedPublisher) {
        throw "Unexpected Authenticode publisher for ${artifact}: ${publisher}"
    }

    $chain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
    try {
        $chain.ChainPolicy.VerificationFlags = `
            [System.Security.Cryptography.X509Certificates.X509VerificationFlags]::IgnoreNotTimeValid
        [void]$chain.Build($signature.SignerCertificate)
        if ($chain.ChainElements.Count -eq 0) {
            throw "Authenticode certificate chain is empty for ${artifact}."
        }
        $rootThumbprint = `
            $chain.ChainElements[$chain.ChainElements.Count - 1].Certificate.Thumbprint
        if ($rootThumbprint -ne $expectedRootThumbprint) {
            throw "Unexpected Authenticode trust root for ${artifact}: ${rootThumbprint}"
        }
    }
    finally {
        $chain.Dispose()
    }
}
