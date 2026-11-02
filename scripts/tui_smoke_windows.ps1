param(
    [string]$Binary = "target\debug\diskdrift.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$tmp = Join-Path $root "target\test-tmp\tui-smoke-windows"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$home = Join-Path $tmp "home"
New-Item -ItemType Directory -Force -Path $home | Out-Null
$env:HOME = $home
$env:USERPROFILE = $home
$config = Join-Path $tmp "config.json"
Set-Content -LiteralPath $config -NoNewline -Value '{"schemaVersion":1}'

# Windows CI cannot use the POSIX pty harness. Exercise the real binary's
# noninteractive guard without creating a default store; native interactive
# restoration is covered by the Windows runner's console host.
& $Binary --config $config --db (Join-Path $tmp "store") tui 2>$null
if ($LASTEXITCODE -eq 0) {
    throw "TUI unexpectedly succeeded without an interactive Windows console"
}
Write-Host "ok  Windows TUI noninteractive guard"
