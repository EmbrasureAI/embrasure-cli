[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$')]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9A-Fa-f]{64}$')]
    [string]$ArchiveSha256,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$OutputDirectory,

    [ValidatePattern('^\d{4}-\d{2}-\d{2}$')]
    [string]$ReleaseDate = [DateTime]::UtcNow.ToString('yyyy-MM-dd')
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

if (Test-Path -LiteralPath $OutputDirectory) {
    throw "Manifest output directory already exists: ${OutputDirectory}"
}

$archiveName = "embrasure-${Version}-x86_64-pc-windows-msvc.zip"
$archiveUrl = "https://github.com/EmbrasureAI/embrasure-cli/releases/download/v${Version}/${archiveName}"
$packageRoot = "embrasure-${Version}-x86_64-pc-windows-msvc"
$replacements = @{
    '@VERSION@' = $Version
    '@SHA256@' = $ArchiveSha256.ToUpperInvariant()
    '@ARCHIVE_URL@' = $archiveUrl
    '@PACKAGE_ROOT@' = $packageRoot
    '@RELEASE_DATE@' = $ReleaseDate
}
$encoding = New-Object Text.UTF8Encoding($false)

foreach ($kind in @('scoop', 'winget')) {
    $sourceDirectory = Join-Path $PSScriptRoot $kind
    $destinationDirectory = Join-Path $OutputDirectory $kind
    New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
    foreach ($template in Get-ChildItem -LiteralPath $sourceDirectory -Filter '*.template' -File) {
        $content = [IO.File]::ReadAllText($template.FullName)
        foreach ($token in $replacements.Keys) {
            $content = $content.Replace($token, $replacements[$token])
        }
        if ($content -match '@[A-Z_]+@') {
            throw "Unresolved template token in $($template.Name)."
        }
        $destinationName = $template.Name.Substring(0, $template.Name.Length - '.template'.Length)
        [IO.File]::WriteAllText((Join-Path $destinationDirectory $destinationName), $content, $encoding)
    }
}
