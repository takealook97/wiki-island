# Runs as a newly created standard (non-administrator) local user. Proves that a per-user
# Task Scheduler logon task can be registered, started, queried, stopped and deleted
# without elevation, then runs the same roundtrip through `wi setup` and `wi uninstall`.
param(
    [Parameter(Mandatory = $true)][string]$Work,
    [string]$Wi = ""
)
$ErrorActionPreference = "Continue"
$failed = $false
function Fail([string]$message) {
    Write-Host "FAIL: $message"
    $script:failed = $true
}
function Run([string]$exe, [string[]]$arguments) {
    Write-Host "> $exe $($arguments -join ' ')"
    $output = & $exe @arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    Write-Host $output.TrimEnd()
    Write-Host "exit $code"
    return @{ Code = $code; Text = $output }
}
function TaskStatus([string]$name) {
    $row = & schtasks.exe /Query /TN $name /FO CSV /V /NH 2>$null | Select-Object -First 1
    if ($LASTEXITCODE -ne 0 -or -not $row) { return "absent" }
    return ($row | ConvertFrom-Csv -Header (1..40)).4
}

Write-Host "== identity"
whoami
$groups = whoami /groups /fo csv /nh | Out-String
Write-Host $groups.TrimEnd()
if ($groups -match "S-1-5-32-544") { Fail "the proof user belongs to Administrators" }
$sid = ((whoami /user /fo csv /nh) | ConvertFrom-Csv -Header Name, Sid).Sid
Write-Host "sid $sid"

Write-Host "== raw schtasks roundtrip"
$name = "wiproof-raw"
$xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>$sid</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>$sid</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Hidden>true</Hidden>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author"><Exec><Command>cmd.exe</Command><Arguments>/c ping -n 600 127.0.0.1</Arguments></Exec></Actions>
</Task>
"@
$file = Join-Path $Work "raw-task.xml"
[System.IO.File]::WriteAllText($file, $xml, [System.Text.Encoding]::Unicode)
if ((Run schtasks.exe @("/Create", "/XML", $file, "/TN", $name, "/F")).Code -ne 0) { Fail "raw /Create" }
if ((Run schtasks.exe @("/Run", "/TN", $name)).Code -ne 0) { Fail "raw /Run" }
Start-Sleep -Seconds 3
$state = TaskStatus $name
Write-Host "raw status: $state"
if ($state -ne "Running") { Fail "raw task is not running" }
[void](Run schtasks.exe @("/End", "/TN", $name))
if ((Run schtasks.exe @("/Delete", "/TN", $name, "/F")).Code -ne 0) { Fail "raw /Delete" }
if ((TaskStatus $name) -ne "absent") { Fail "raw task still exists" }

if ($Wi) {
    Write-Host "== wi setup roundtrip"
    $env:WI_CACHE_DIR = Join-Path $Work "cache"
    if ((Run $Wi @("setup")).Code -ne 0) { Fail "wi setup" }
    $tasks = & schtasks.exe /Query /FO CSV /NH 2>$null | ConvertFrom-Csv -Header Name, Next, Status |
        Where-Object { $_.Name -like "\wiki-island-*" }
    Write-Host ($tasks | Format-Table | Out-String).TrimEnd()
    if (@($tasks).Count -ne 1) { Fail "expected one wiki-island task" }
    $task = @($tasks)[0].Name.TrimStart("\")
    [void](Run schtasks.exe @("/Query", "/TN", $task, "/XML"))
    Start-Sleep -Seconds 5
    $state = TaskStatus $task
    Write-Host "task status: $state"
    if ($state -ne "Running") { Fail "wi task is not running" }
    $watchers = @(Get-Process -Name wi -ErrorAction SilentlyContinue)
    Write-Host "wi processes: $($watchers.Count)"
    if ($watchers.Count -lt 1) { Fail "no watcher process" }
    $doctor = Run $Wi @("doctor")
    if ($doctor.Text -notmatch "\[ok\] service:") { Fail "doctor does not report the service as ok" }
    if ((Run $Wi @("uninstall")).Code -ne 0) { Fail "wi uninstall" }
    Start-Sleep -Seconds 2
    if ((TaskStatus $task) -ne "absent") { Fail "wi task still exists" }
    $left = @(Get-Process -Name wi -ErrorAction SilentlyContinue)
    Write-Host "wi processes after uninstall: $($left.Count)"
    if ($left.Count -ne 0) { Fail "watcher still running after uninstall" }
}

if ($failed) { Write-Host "RESULT: FAILED"; exit 1 }
Write-Host "RESULT: PASSED"
exit 0
