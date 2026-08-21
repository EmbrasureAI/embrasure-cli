[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$BinaryPath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$')]
    [string]$Version
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$metadata = (Get-Item -LiteralPath $BinaryPath).VersionInfo
$expected = @{
    ProductName = 'Embrasure'
    ProductVersion = $Version
    FileVersion = $Version
    CompanyName = 'Embrasure, Inc.'
    FileDescription = 'Validate dbt changes against production Snowflake data'
    OriginalFilename = 'embrasure.exe'
    LegalCopyright = 'Copyright 2026 Embrasure, Inc.'
}
foreach ($field in $expected.Keys) {
    if ($metadata.$field -ne $expected[$field]) {
        throw "Invalid PE ${field}: '$($metadata.$field)' (expected '$($expected[$field])')."
    }
}
