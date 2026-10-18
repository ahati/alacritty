<#
.SYNOPSIS
    Generate winget manifest files for this Alacritty fork.

.DESCRIPTION
    Renders the YAML templates in .\templates into a manifest directory that
    `winget install --manifest` can read.

    winget requires the installer to be downloadable, so the manifest has to name
    a URL: the release assets published by .github/workflows/release.yml fit,
    which are built when a tag matching v<major>.<minor>.<patch> is pushed.

    The SHA256 is taken from the artifact itself. By default the artifact is
    downloaded from the very URL the manifest will name, so the hash cannot drift
    from what winget will fetch; pass -Artifact to hash a local file instead.

.EXAMPLE
    # Release v0.18.0 was published by the workflow; write manifests for the
    # portable executable (no administrator rights needed to install).
    .\generate.ps1 -Version 0.18.0

.EXAMPLE
    # Hash a local build instead of downloading, and emit an MSI manifest.
    .\generate.ps1 -Version 0.18.0 -Installer msi -Artifact .\Alacritty-v0.18.0-installer.msi `
        -ProductCode '{4A1F7E7C-...}' -UpgradeCode '{0E0F0C48-...}'
#>
[CmdletBinding()]
param(
    # Package version, without a suffix: 0.18.0, not 0.18.0-dev.
    [Parameter(Mandatory = $true)]
    [string] $Version,

    # Release tag, which is what the release workflow's asset names are built
    # from. Defaults to v<Version>.
    [string] $Tag,

    # Which artifact the manifest installs.
    [ValidateSet('portable', 'msi')]
    [string] $Installer = 'portable',

    # Repository that publishes the release assets.
    [string] $Repository = 'ahati/alacritty',

    # Package identity, which must differ from upstream's Alacritty.Alacritty.
    [string] $PackageIdentifier = 'ahati.Alacritty',

    # Moniker must differ from upstream's `alacritty`, or `winget install
    # alacritty` becomes ambiguous once both manifests are known locally.
    [string] $Moniker = 'alacritty-fork',

    [string] $PackageName = 'Alacritty (fork)',
    [string] $Publisher = 'ahati',
    [string] $ShortDescription = 'A fork of Alacritty with Kitty graphics protocol support for inline images.',

    # Local artifact to hash, instead of downloading the release asset.
    [string] $Artifact,

    # Hash supplied directly, for a manifest that names an artifact you already
    # published.
    [string] $Sha256,

    # MSI identity, required for -Installer msi.
    [string] $ProductCode,
    [string] $UpgradeCode,

    # Directory the manifest files are written to.
    [string] $OutDir
)

$ErrorActionPreference = 'Stop'

function Fail([string] $message) {
    Write-Error $message
    exit 1
}

# winget compares versions numerically, and the MSI carries this version too,
# where anything but digits and dots is invalid.
if ($Version -notmatch '^\d+(\.\d+){0,3}$') {
    Fail "Version '$Version' is not numeric. Use the release version (e.g. 0.18.0), not the development suffix."
}

if (-not $Tag) {
    $Tag = "v$Version"
}

$asset = if ($Installer -eq 'msi') {
    "Alacritty-$Tag-installer.msi"
} else {
    "Alacritty-$Tag-portable.exe"
}

$installerUrl = "https://github.com/$Repository/releases/download/$Tag/$asset"

if ($Installer -eq 'msi') {
    if (-not $ProductCode) {
        Fail "-Installer msi needs -ProductCode, the MSI's ProductCode from its build (it is generated per build)."
    }
    if (-not $UpgradeCode) {
        Fail @"
-Installer msi needs -UpgradeCode.

The installer definition at alacritty/windows/wix/alacritty.wxs still carries
upstream's UpgradeCode (87c21c74-dbd5-4584-89d5-46d9cd0c40a7), so Windows
Installer treats this fork as the same product as upstream Alacritty and will
replace it. Give the fork its own UpgradeCode before publishing an MSI manifest.
"@
    }
}

if (-not $Sha256) {
    if ($Artifact) {
        if (-not (Test-Path $Artifact)) {
            Fail "Artifact not found: $Artifact"
        }
        $Sha256 = (Get-FileHash -Path $Artifact -Algorithm SHA256).Hash
        Write-Host "Hashed    $Artifact"
    } else {
        $download = Join-Path ([System.IO.Path]::GetTempPath()) $asset
        Write-Host "Downloading $installerUrl"
        Invoke-WebRequest -Uri $installerUrl -OutFile $download -UseBasicParsing
        $Sha256 = (Get-FileHash -Path $download -Algorithm SHA256).Hash
        Remove-Item $download -Force
        Write-Host "Hashed    the release asset"
    }
}

if (-not $OutDir) {
    $OutDir = Join-Path $PSScriptRoot 'manifests'
}

$templates = Join-Path $PSScriptRoot 'templates'
$repositoryUrl = "https://github.com/$Repository"

$tokens = @{
    '{{PACKAGE_IDENTIFIER}}' = $PackageIdentifier
    '{{VERSION}}'            = $Version
    '{{PUBLISHER}}'          = $Publisher
    '{{REPOSITORY_URL}}'     = $repositoryUrl
    '{{PACKAGE_NAME}}'       = $PackageName
    '{{SHORT_DESCRIPTION}}'  = $ShortDescription
    '{{MONIKER}}'            = $Moniker
    '{{INSTALLER_URL}}'      = $installerUrl
    '{{INSTALLER_SHA256}}'   = $Sha256.ToUpperInvariant()
    '{{RELEASE_DATE}}'       = (Get-Date -Format 'yyyy-MM-dd')
    '{{PRODUCT_CODE}}'       = $ProductCode
    '{{UPGRADE_CODE}}'       = $UpgradeCode
}

# winget reads every YAML file in the directory, so the set is rendered from
# scratch each time to avoid leaving a stale manifest behind.
if (Test-Path $OutDir) {
    Get-ChildItem -Path $OutDir -Filter '*.yaml' | Remove-Item -Force
}
else {
    New-Item -ItemType Directory -Path $OutDir | Out-Null
}

$sources = [ordered]@{
    'version.yaml'             = "$PackageIdentifier.yaml"
    'locale.en-US.yaml'        = "$PackageIdentifier.locale.en-US.yaml"
    "installer.$Installer.yaml" = "$PackageIdentifier.installer.yaml"
}

foreach ($source in $sources.Keys) {
    $path = Join-Path $templates $source
    if (-not (Test-Path $path)) {
        Fail "Missing template: $path"
    }

    $text = Get-Content -Path $path -Raw
    foreach ($token in $tokens.Keys) {
        $text = $text.Replace($token, [string] $tokens[$token])
    }

    $target = Join-Path $OutDir $sources[$source]
    [System.IO.File]::WriteAllText($target, $text, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "Wrote     $target"
}

Write-Host @"

Manifests are ready in $OutDir

Install it (once per machine, in an administrator shell):
  winget settings --enable LocalManifestFiles

Then, as the user who wants the terminal:
  winget install --manifest "$OutDir"

Remove it again with:
  winget uninstall --id $PackageIdentifier
"@
