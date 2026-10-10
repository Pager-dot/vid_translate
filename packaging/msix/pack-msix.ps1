<#
.SYNOPSIS
  Stage a Tauri release build into an MSIX layout and pack it for the Microsoft Store.

.DESCRIPTION
  Tauri has no MSIX bundle target, so this assembles the layout by hand from the plain
  release binary (NOT from the NSIS/MSI installer — the Store wants the app, not a setup
  program) and calls makeappx from the Windows SDK.

  The package is left UNSIGNED on purpose: the Store signs submissions itself, which is the
  whole reason this project goes the MSIX route rather than paying for a code-signing
  certificate. Pass -SelfSign to get a locally installable package for testing.

  Identity values come from Partner Center and are passed in, never committed.

.EXAMPLE
  pwsh packaging/msix/pack-msix.ps1 -IdentityName 1234Publisher.VidTranslate `
       -Publisher "CN=ABCD1234-..." -PublisherDisplayName "Your Name"
#>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$IdentityName,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$PublisherDisplayName,
  # Must match a name reserved in Partner Center under "Manage app names", exactly.
  [string]$DisplayName = "VidTranslate",
  [string]$OutFile = "VidTranslate.msix",
  [switch]$SelfSign
)

$ErrorActionPreference = "Stop"
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$release  = Join-Path $repoRoot "src-tauri\target\release"
$stage    = Join-Path $repoRoot "target\msix-stage"

# --- version: three parts from package.json, fourth always 0 (a Store rule) ---
$version = (Get-Content (Join-Path $repoRoot "package.json") -Raw | ConvertFrom-Json).version
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "package.json version '$version' is not three-part semver" }
$msixVersion = "$version.0"
Write-Host "Packaging VidTranslate $msixVersion"

# --- stage ---
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $stage "Assets") -Force | Out-Null

# `tauri build --no-bundle` leaves the raw Cargo artifact, which is named after the bin
# target (vid_translate.exe) - only the bundler renames it to productName. The manifest's
# Application/@Executable says VidTranslate.exe, so rename on the way into the stage.
$exe = @("VidTranslate.exe", "vid_translate.exe") |
  ForEach-Object { Join-Path $release $_ } |
  Where-Object { Test-Path $_ } |
  Select-Object -First 1
if (-not $exe) { throw "no release binary in $release - run ``npm run tauri build`` first" }
Write-Host "Using $exe"
Copy-Item $exe (Join-Path $stage "VidTranslate.exe")

# The four vendored DLLs sit beside the exe, not in a resources\ subfolder: libvosk is
# linked at load time, and Windows resolves that from the executable's own directory.
foreach ($dll in @("libvosk.dll", "libgcc_s_seh-1.dll", "libstdc++-6.dll", "libwinpthread-1.dll")) {
  $src = Join-Path $repoRoot "src-tauri\$dll"
  if (-not (Test-Path $src)) { throw "missing vendored DLL: $src" }
  Copy-Item $src $stage
}

# Reuse the icon set the Tauri bundler already generates from the app icon.
$icons = Join-Path $repoRoot "src-tauri\icons"
foreach ($logo in @("Square44x44Logo.png", "Square71x71Logo.png", "Square150x150Logo.png", "StoreLogo.png")) {
  Copy-Item (Join-Path $icons $logo) (Join-Path $stage "Assets")
}

# --- manifest ---
$manifest = Get-Content (Join-Path $PSScriptRoot "AppxManifest.xml") -Raw
$manifest = $manifest.Replace("@IDENTITY_NAME@", $IdentityName).
                      Replace("@PUBLISHER@", $Publisher).
                      Replace("@PUBLISHER_DISPLAY_NAME@", $PublisherDisplayName).
                      Replace("@DISPLAY_NAME@", $DisplayName).
                      Replace("@VERSION@", $msixVersion)
Set-Content -Path (Join-Path $stage "AppxManifest.xml") -Value $manifest -Encoding UTF8

# --- pack ---
$sdkBin = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory |
  Where-Object { Test-Path (Join-Path $_.FullName "x64\makeappx.exe") } |
  Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdkBin) { throw "makeappx.exe not found - install the Windows 10/11 SDK" }
$makeappx = Join-Path $sdkBin.FullName "x64\makeappx.exe"

$out = Join-Path $repoRoot $OutFile
if (Test-Path $out) { Remove-Item $out -Force }
& $makeappx pack /d $stage /p $out /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed with $LASTEXITCODE" }

if ($SelfSign) {
  # Local testing only. A self-signed package installs only after its certificate is
  # trusted on that machine; Store submissions must go up unsigned.
  $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
    -KeyUsage DigitalSignature -CertStoreLocation "Cert:\CurrentUser\My" `
    -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
  $signtool = Join-Path $sdkBin.FullName "x64\signtool.exe"
  & $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $out
  if ($LASTEXITCODE -ne 0) { throw "signtool failed with $LASTEXITCODE" }
  Write-Host "Self-signed with $($cert.Thumbprint) - trust this certificate to install locally."
}

Write-Host "Packed $out"
