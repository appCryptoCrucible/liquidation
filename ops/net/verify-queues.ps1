# Verify feed vs submission NIC queues. Fail closed.
# Exit 0 = PASS, 1 = FAIL, 2 = ABSENT. Never PASS when /sys is missing.
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$Applied = Join-Path $Root "state.applied"

Write-Output "liq-net-queues: verify"

if (-not (Test-Path -LiteralPath "/sys" -PathType Container)) {
    Write-Output "ABSENT: /sys is not a directory - cannot evaluate NIC queues"
    exit 2
}

if (-not (Test-Path -LiteralPath "/sys/class/net" -PathType Container)) {
    Write-Output "ABSENT: /sys/class/net missing"
    exit 2
}

if (-not (Test-Path -LiteralPath $Applied -PathType Leaf)) {
    Write-Output "FAIL: state.applied missing - queues.sh has not succeeded (not PASS)"
    exit 1
}

Write-Output "FAIL: PowerShell verify on a host with /sys must use verify-queues.sh"
exit 1
