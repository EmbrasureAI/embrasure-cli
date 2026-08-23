[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$BinaryPath,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$OutputDirectory
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

if (Test-Path -LiteralPath $OutputDirectory) {
    throw "Payload directory already exists: ${OutputDirectory}"
}

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$binDirectory = Join-Path $OutputDirectory 'bin'
$libexecDirectory = Join-Path $OutputDirectory 'libexec\embrasure'
$pythonDirectory = Join-Path $libexecDirectory 'python'
$docsDirectory = Join-Path $OutputDirectory 'docs'
New-Item -ItemType Directory -Path $binDirectory, $pythonDirectory, $docsDirectory -Force | Out-Null

Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $binDirectory 'embrasure.exe')

$documents = @('LICENSE', 'NOTICE', 'README.md', 'SECURITY.md', 'embrasure-check.example.yml')
foreach ($document in $documents) {
    Copy-Item -LiteralPath (Join-Path $repositoryRoot $document) -Destination $docsDirectory
}
Copy-Item -LiteralPath (Join-Path $repositoryRoot 'docs') -Destination (Join-Path $docsDirectory 'reference') -Recurse

& python -m pip download `
    --only-binary=:all: `
    --no-deps `
    --require-hashes `
    --dest $pythonDirectory `
    -r (Join-Path $repositoryRoot 'packaging\sqlglot-requirements.txt')
if ($LASTEXITCODE -ne 0) {
    throw "Could not download the pinned SQLGlot wheel (exit ${LASTEXITCODE})."
}
$wheels = @(Get-ChildItem -LiteralPath $pythonDirectory -Filter 'sqlglot-*.whl' -File)
if ($wheels.Count -ne 1 -or $wheels[0].Name -ne 'sqlglot-30.7.0-py3-none-any.whl') {
    throw 'Windows payload must contain exactly the pinned SQLGlot 30.7.0 wheel.'
}
