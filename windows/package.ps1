[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$Version,
    [string]$RepositoryRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$OutputDirectory = (Join-Path $PSScriptRoot 'dist'),
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$SigningCertificateThumbprint
)

$ErrorActionPreference = 'Stop'
$vvmuxRoot = Join-Path $RepositoryRoot 'vvmux'
$releaseBinary = Join-Path $vvmuxRoot 'target\release\vvmux.exe'
if (-not (Test-Path -LiteralPath $releaseBinary -PathType Leaf)) {
    throw "Release binary not found: $releaseBinary"
}

$staging = Join-Path $OutputDirectory "vvmux-$Version-x86_64-pc-windows-msvc"
if (Test-Path -LiteralPath $staging) {
    Remove-Item -LiteralPath $staging -Recurse
}
New-Item -ItemType Directory -Path $staging -Force | Out-Null

Copy-Item -LiteralPath $releaseBinary -Destination (Join-Path $staging 'vvmux.exe')
Copy-Item -LiteralPath (Join-Path $vvmuxRoot 'README.md') -Destination $staging
Copy-Item -LiteralPath (Join-Path $RepositoryRoot 'vivido\LICENSE') -Destination (Join-Path $staging 'LICENSE')
Copy-Item -LiteralPath (Join-Path $vvmuxRoot 'config.example.toml') -Destination $staging
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1') -Destination $staging
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'uninstall.ps1') -Destination $staging

$certificate = Get-Item "Cert:\CurrentUser\My\$SigningCertificateThumbprint"
foreach ($script in @('install.ps1', 'uninstall.ps1')) {
    $signature = Set-AuthenticodeSignature -FilePath (Join-Path $staging $script) -Certificate $certificate -HashAlgorithm SHA256
    if ($signature.Status -ne 'Valid') {
        throw "Authenticode signing failed for ${script}: $($signature.StatusMessage)"
    }
}

$archive = Join-Path $OutputDirectory "vvmux-$Version-x86_64-pc-windows-msvc.zip"
if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive
}
Compress-Archive -LiteralPath $staging -DestinationPath $archive -CompressionLevel Optimal
Write-Output $archive
