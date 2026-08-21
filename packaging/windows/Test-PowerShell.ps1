Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

$scripts = @(
    $PSCommandPath,
    (Join-Path $PSScriptRoot '..\..\install.ps1'),
    (Join-Path $PSScriptRoot 'Stage-Payload.ps1'),
    (Join-Path $PSScriptRoot 'Test-PeMetadata.ps1'),
    (Join-Path $PSScriptRoot 'Test-Installer.ps1'),
    (Join-Path $PSScriptRoot 'update-helper.ps1'),
    (Join-Path $PSScriptRoot 'Test-Signature.ps1')
)
foreach ($script in $scripts) {
    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) {
        throw "PowerShell parse errors in ${script}: $($errors -join '; ')"
    }
}

$updateSource = Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\..\src\update.rs') -Raw
if ($updateSource -notmatch '"-ExecutionPolicy",\s*\r?\n\s*"Bypass"' -or
    $updateSource -match '"-ExecutionPolicy",\s*\r?\n\s*"AllSigned"') {
    throw 'The internal update helper must use the verified non-interactive process policy.'
}

. (Join-Path $PSScriptRoot '..\..\install.ps1')

$policyMessage = Get-WindowsInstallerFailureMessage -ExitCode 1625 -LogPath 'installer.log'
if ($policyMessage -notmatch 'current-user MSI' -or
    $policyMessage -notmatch 'DisableMSI=0' -or
    $policyMessage -notmatch 'installer\.log') {
    throw 'Windows Installer policy guidance is incomplete.'
}

Assert-EmbrasureSignaturePolicy `
    -Status ([System.Management.Automation.SignatureStatus]::Valid) `
    -Publisher 'Embrasure, Inc.' `
    -RootThumbprint 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2' `
    -Path 'valid-fixture'
$rejectedSignatures = @(
    @{ Status = [System.Management.Automation.SignatureStatus]::HashMismatch; Publisher = 'Embrasure, Inc.'; Root = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'; Name = 'altered script or MSI' },
    @{ Status = [System.Management.Automation.SignatureStatus]::NotSigned; Publisher = ''; Root = ''; Name = 'unsigned artifact' },
    @{ Status = [System.Management.Automation.SignatureStatus]::NotTrusted; Publisher = 'Embrasure, Inc.'; Root = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'; Name = 'expired or untrusted certificate' },
    @{ Status = [System.Management.Automation.SignatureStatus]::Valid; Publisher = 'Unexpected Publisher'; Root = 'F40042E2E5F7E8EF8189FED15519AECE42C3BFA2'; Name = 'wrong publisher' },
    @{ Status = [System.Management.Automation.SignatureStatus]::Valid; Publisher = 'Embrasure, Inc.'; Root = '0000000000000000000000000000000000000000'; Name = 'wrong trust root' }
)
foreach ($fixture in $rejectedSignatures) {
    try {
        Assert-EmbrasureSignaturePolicy `
            -Status $fixture.Status `
            -Publisher $fixture.Publisher `
            -RootThumbprint $fixture.Root `
            -Path $fixture.Name
        throw "Signature policy accepted $($fixture.Name)."
    }
    catch {
        if ($_.Exception.Message -eq "Signature policy accepted $($fixture.Name).") { throw }
    }
}

if ((Resolve-EmbrasureVersion -RequestedVersion '1.2.3') -ne '1.2.3') {
    throw 'Version validation failed.'
}
foreach ($invalidVersion in @('v1.2.3', '1.2', '1.2.3-beta', '01.2.3', 'vv1.2.3', '../1.2.3')) {
    try {
        Resolve-EmbrasureVersion -RequestedVersion $invalidVersion | Out-Null
        throw "Malformed version was accepted: ${invalidVersion}"
    }
    catch {
        if ($_.Exception.Message -eq "Malformed version was accepted: ${invalidVersion}") { throw }
    }
}

$checksumFile = Join-Path ([IO.Path]::GetTempPath()) ("embrasure-checksums-test-$([guid]::NewGuid()).txt")
$validHash = 'a' * 64
Set-Content -LiteralPath $checksumFile -Value "${validHash}  artifact.msi" -Encoding ASCII
if ((Read-ExpectedChecksum -ChecksumPath $checksumFile -FileName 'artifact.msi') -ne $validHash) {
    throw 'Checksum parsing failed.'
}
Add-Content -LiteralPath $checksumFile -Value "${validHash}  artifact.msi" -Encoding ASCII
try {
    Read-ExpectedChecksum -ChecksumPath $checksumFile -FileName 'artifact.msi' | Out-Null
    throw 'Duplicate checksum was accepted.'
}
catch {
    if ($_.Exception.Message -eq 'Duplicate checksum was accepted.') { throw }
}

Set-Content -LiteralPath $checksumFile -Value "${validHash}  artifact.msi" -Encoding ASCII
Add-Content -LiteralPath $checksumFile -Value 'not-a-hash  artifact.msi' -Encoding ASCII
try {
    Read-ExpectedChecksum -ChecksumPath $checksumFile -FileName 'artifact.msi' | Out-Null
    throw 'Malformed duplicate checksum was accepted.'
}
catch {
    if ($_.Exception.Message -eq 'Malformed duplicate checksum was accepted.') { throw }
}

Set-Content -LiteralPath $checksumFile -Value "not-a-hash  artifact.msi" -Encoding ASCII
try {
    Read-ExpectedChecksum -ChecksumPath $checksumFile -FileName 'artifact.msi' | Out-Null
    throw 'Malformed checksum was accepted.'
}
catch {
    if ($_.Exception.Message -eq 'Malformed checksum was accepted.') { throw }
}

$hashFixture = Join-Path ([IO.Path]::GetTempPath()) ("embrasure-hash-test-$([guid]::NewGuid()).msi")
try {
    Set-Content -LiteralPath $hashFixture -Value 'original package' -Encoding ASCII
    $expectedHash = (Get-FileHash -LiteralPath $hashFixture -Algorithm SHA256).Hash
    Assert-EmbrasureHash -Path $hashFixture -ExpectedSha256 $expectedHash
    Add-Content -LiteralPath $hashFixture -Value 'post-signing mutation' -Encoding ASCII
    try {
        Assert-EmbrasureHash -Path $hashFixture -ExpectedSha256 $expectedHash
        throw 'Altered MSI passed checksum verification.'
    }
    catch {
        if ($_.Exception.Message -eq 'Altered MSI passed checksum verification.') { throw }
    }
}
finally {
    Remove-Item -LiteralPath $hashFixture -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $checksumFile -Force -ErrorAction SilentlyContinue
}
