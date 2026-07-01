#Requires -Version 5.1
<#
.SYNOPSIS
  Download and verify the official WinTun driver DLL (wintun.net), then copy
  the requested architecture's wintun.dll to -OutFile.

.DESCRIPTION
  Per docs/vpn/VPN_WINDOWS.md, bore bundles the official signed wintun.dll
  redistributable (permitted by WinTun's own distribution terms: "the below
  signed DLLs are the only supported way of distributing Wintun") rather than
  requiring users to fetch it themselves. The version + zip SHA256 are PINNED
  below — bumping the WinTun version requires updating both here.

  Used by:
    - .github/workflows/ci.yml (windows-vpn-e2e): places wintun.dll next to
      the compiled example/binary so the default DLL search path finds it.
    - .github/workflows/mean_bean_deploy.yml (Windows release packaging):
      bundles the correct arch's DLL alongside bore.exe in the release zip.

.PARAMETER Arch
  WinTun architecture folder inside the release zip: amd64, x86, arm64, arm.

.PARAMETER OutFile
  Destination path for wintun.dll.
#>
param(
    [ValidateSet('amd64', 'x86', 'arm64', 'arm')]
    [string]$Arch = 'amd64',
    [Parameter(Mandatory = $true)]
    [string]$OutFile
)

$ErrorActionPreference = 'Stop'

# Pinned WinTun release (bump both together; see docs/vpn/VPN_WINDOWS.md).
$WintunVersion = '0.14.1'
$WintunUrl = "https://www.wintun.net/builds/wintun-$WintunVersion.zip"
$WintunZipSha256 = '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51'

$tempZip = Join-Path $env:TEMP "wintun-$WintunVersion.zip"
$tempExtract = Join-Path $env:TEMP "wintun-$WintunVersion-extract"

Write-Host "Downloading $WintunUrl ..."
Invoke-WebRequest -Uri $WintunUrl -OutFile $tempZip -UseBasicParsing

$actualSha256 = (Get-FileHash -Path $tempZip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualSha256 -ne $WintunZipSha256) {
    throw "wintun-$WintunVersion.zip SHA256 mismatch: expected $WintunZipSha256, got $actualSha256"
}
Write-Host "SHA256 verified: $actualSha256"

if (Test-Path $tempExtract) {
    Remove-Item -Recurse -Force $tempExtract
}
Expand-Archive -Path $tempZip -DestinationPath $tempExtract

$dllPath = Join-Path $tempExtract "wintun\bin\$Arch\wintun.dll"
if (-not (Test-Path $dllPath)) {
    throw "Expected DLL not found at $dllPath after extraction"
}

$outDir = Split-Path -Parent $OutFile
if ($outDir -and -not (Test-Path $outDir)) {
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
}
Copy-Item -Path $dllPath -Destination $OutFile -Force
Write-Host "wintun.dll ($Arch) written to $OutFile"
