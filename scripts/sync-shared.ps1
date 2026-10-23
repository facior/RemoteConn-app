<#
.SYNOPSIS
  Copies the protocol crates shared with the server into the server repository,
  or checks that both copies are identical.

.DESCRIPTION
  rc-proto, rc-crypto, rc-transport and rc-rendezvous exist in both
  repositories, so each builds on its own (the server on a VPS without access to
  this repository). This repository is the source of truth: change the crates
  here, then run this script and commit the result in the server repository.

.EXAMPLE
  .\scripts\sync-shared.ps1                  # copy to ..\RemoteConn-server
  .\scripts\sync-shared.ps1 -Check           # only compare (exit code 1 = differ)
  .\scripts\sync-shared.ps1 -ServerRepo D:\src\RemoteConn-server
#>
param(
    [string]$ServerRepo = (Join-Path $PSScriptRoot '..\..\RemoteConn-server'),
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
$shared = 'rc-proto', 'rc-crypto', 'rc-transport', 'rc-rendezvous'
$appRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
if (-not (Test-Path (Join-Path $ServerRepo 'rc-server'))) {
    throw "Server repository not found at $ServerRepo (use -ServerRepo)."
}

function Get-Tree($dir) {
    $base = (Resolve-Path $dir).Path
    Get-ChildItem $dir -Recurse -File |
        Where-Object { $_.FullName -notmatch '\\target\\' } |
        ForEach-Object {
            [pscustomobject]@{
                Path = $_.FullName.Substring($base.Length + 1)
                Hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
            }
        }
}

$different = @()
foreach ($crate in $shared) {
    $from = Join-Path $appRoot "crates\$crate"
    $to = Join-Path $ServerRepo "crates\$crate"
    if ($Check) {
        $a = @(Get-Tree $from)
        $b = if (Test-Path $to) { @(Get-Tree $to) } else { @() }
        $diff = Compare-Object $a $b -Property Path, Hash
        if ($diff) { $different += $crate; $diff | ForEach-Object { "  $crate\$($_.Path) ($($_.SideIndicator))" } }
    } else {
        robocopy $from $to /MIR /XD target /NFL /NDL /NJH /NJS /NP | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "robocopy failed for $crate ($LASTEXITCODE)" }
        "copied $crate"
    }
}

if ($Check) {
    if ($different.Count) {
        "Shared crates differ: $($different -join ', '). Run scripts\sync-shared.ps1."
        exit 1
    }
    'Shared crates are identical.'
}
