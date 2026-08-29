$ErrorActionPreference = 'Stop'

$packageName = 'shep'
$toolsDir    = Split-Path -Parent $MyInvocation.MyCommand.Definition
$shep        = Join-Path $toolsDir 'shep.exe'

# Refuse while a shepherd is up, rather than stopping the flock on the
# operator's behalf.
#
# Two reasons, and the second is the one that decides it. First, shep is a
# process manager: an uninstall that silently kills whatever it was
# supervising is a surprise nobody asked for, and 'choco uninstall' runs
# unattended often enough that the operator may not be watching. Second,
# Windows will not delete a running executable, so leaving the shepherd up
# turns this into a confusing partial uninstall a few lines further down. A
# clear refusal is strictly better than that failure.
#
# 'shep ping' exits non-zero when nothing answers, and it is the only verb
# that treats "no shepherd" as information rather than an error, so it is the
# right probe here.
if (Test-Path $shep) {
  & $shep ping *> $null
  if ($LASTEXITCODE -eq 0) {
    throw "A shepherd is still running. Run 'shep kill' to shut it down, then uninstall again. Uninstalling will not stop your flock for you."
  }
}

Uninstall-ChocolateyZipPackage -PackageName $packageName -ZipFileName 'shep-x86_64-pc-windows-msvc.zip'
