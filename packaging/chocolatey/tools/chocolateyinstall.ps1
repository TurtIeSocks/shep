$ErrorActionPreference = 'Stop'

$packageName = 'shep'
$toolsDir    = Split-Path -Parent $MyInvocation.MyCommand.Definition
$version     = $env:ChocolateyPackageVersion

# shep publishes one Windows target, x86_64-pc-windows-msvc. There is no
# 32-bit build to fall back to, so say that plainly rather than letting
# Install-ChocolateyZipPackage fail on a missing Url.
if (-not [Environment]::Is64BitOperatingSystem) {
  throw "shep ships a 64-bit build only. This machine reports a 32-bit operating system."
}

# CHECKSUM64 is substituted at pack time from the SHA256 the release workflow
# published beside the archive. The sentinel below never ships: the packaging
# workflow replaces it and then fails the build if any sentinel survives.
$checksum64 = '__CHECKSUM64__'

$url64 = "https://github.com/shep-pm/shep/releases/download/shep-v$version/shep-x86_64-pc-windows-msvc.zip"

$packageArgs = @{
  packageName    = $packageName
  unzipLocation  = $toolsDir
  url64bit       = $url64
  checksum64     = $checksum64
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs

# shep.exe is shimmed automatically because it lands in the tools directory.
# shep-runtime.exe and shep-dev.exe are not: each has a .ignore file shipped
# beside it, which is what tells shimgen to skip it. They are container
# entrypoint aliases with no desktop use case, and three shims on PATH for one
# tool is noise.
Write-Host "shep $version installed. Run 'shep welcome' for the tour."
Write-Host "Boot-time supervision is not built on Windows: 'shep startup' refuses. See https://shep-pm.com/docs/not-built"
