<#
.SYNOPSIS
  Sets the app version everywhere it is stored.

.DESCRIPTION
  The updater compares desktop/src-tauri/tauri.conf.json with the newest
  release; the service check compares the Rust version (Cargo.toml) of the app
  and of the installed service. Both must match the release tag.

.EXAMPLE
  .\scripts\bump-version.ps1 0.3.0
  git commit -am "Version 0.3.0"; git tag v0.3.0; git push origin main v0.3.0
#>
param([Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$utf8 = New-Object Text.UTF8Encoding $false
$files = @{
    'Cargo.toml'                         = '(?m)^version = "\d+\.\d+\.\d+"'
    'desktop\src-tauri\tauri.conf.json'  = '"version": "\d+\.\d+\.\d+"'
    'desktop\package.json'               = '"version": "\d+\.\d+\.\d+"'
}
foreach ($rel in $files.Keys) {
    $path = Join-Path $root $rel
    $text = [IO.File]::ReadAllText($path)
    $replacement = if ($rel -eq 'Cargo.toml') { "version = `"$Version`"" } else { "`"version`": `"$Version`"" }
    $new = [regex]::new($files[$rel]).Replace($text, $replacement, 1)
    if ($new -eq $text -and $text -notmatch [regex]::Escape($replacement)) { throw "version not found in $rel" }
    [IO.File]::WriteAllText($path, $new, $utf8)
    "$rel -> $Version"
}
