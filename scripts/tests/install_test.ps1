# Tests scripts/install.ps1 against a local HTTP server, a fake release and temporary user folders.
#
# Windows only. Run it with the PowerShell edition under test:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/tests/install_test.ps1
#   pwsh -NoProfile -File scripts/tests/install_test.ps1
# It needs Python 3 on the path (for scripts/tests/release_server.py) and the .NET Framework C#
# compiler, which builds the fake wi.exe and wiki-island.exe. The installer writes the user Path in
# the registry; the test saves its text and value kind first and puts both back at the end.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Here = $PSScriptRoot
$Install = (Resolve-Path -LiteralPath (Join-Path $Here '..\install.ps1')).Path
$HostExe = (Get-Process -Id $PID).Path
$Work = Join-Path ([IO.Path]::GetTempPath()) ("wi-install-test-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
$Local = Join-Path $Work 'local'
$UserHome = Join-Path $Work 'home'
$Tmp = Join-Path $Work 'tmp'
$Site = Join-Path $Work 'site'
$Parent = Join-Path $Local 'Programs\wiki-island'
$Bin = Join-Path $Parent 'bin'
$Tag = 'v9.9.9'
$Asset = "wiki-island-$Tag-x86_64-pc-windows-msvc.zip"
$Rel = Join-Path $Site "releases\download\$Tag"
$Zip = Join-Path $Rel $Asset
$Manifest = Join-Path $Rel 'SHA256SUMS'

$Failed = 0
$Rc = 0
$Out = ''
$Server = $null
$Running = $null
$Entries = @()

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

# Runs program $File with $Arguments and environment changes $Environment (a $null value removes
# the variable). Returns its exit code and its combined output.
function Invoke-Program {
    param([string]$File, [string]$Arguments, [hashtable]$Environment = @{}, [int]$Seconds = 300)
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $File
    $info.Arguments = $Arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($name in @($Environment.Keys)) {
        $value = $Environment[$name]
        if ($null -eq $value) {
            $info.EnvironmentVariables.Remove($name)
        } else {
            $info.EnvironmentVariables[$name] = [string]$value
        }
    }
    $process = [System.Diagnostics.Process]::Start($info)
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit($Seconds * 1000)) {
        $process.Kill()
        throw "$File did not finish within $Seconds seconds."
    }
    $process.WaitForExit()
    return @{ Code = $process.ExitCode; Output = $stdout.Result + $stderr.Result }
}

# Makes $Path an empty folder.
function Initialize-Folder {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) { Remove-Item -LiteralPath $Path -Recurse -Force }
    [void](New-Item -ItemType Directory -Path $Path -Force)
}

# Returns the SHA-256 of file $Path in lower case.
function Get-Digest {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# Returns the user Path as stored: whether it exists, its text unexpanded and its value kind.
function Get-UserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        if ($key.GetValueNames() -contains 'Path') {
            return @{
                Exists = $true
                Kind = $key.GetValueKind('Path')
                Value = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            }
        }
        return @{ Exists = $false; Kind = $null; Value = $null }
    } finally {
        $key.Close()
    }
}

# Stores user Path state $State (as returned by Get-UserPath).
function Write-UserPath {
    param([hashtable]$State)
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        if ($State.Exists) {
            $key.SetValue('Path', $State.Value, $State.Kind)
        } else {
            $key.DeleteValue('Path', $false)
        }
    } finally {
        $key.Close()
    }
}

# Returns how many entries of the stored user Path name the program folder.
function Get-PathEntryCount {
    $state = Get-UserPath
    if (-not $state.Exists) { return 0 }
    $count = 0
    foreach ($entry in $state.Value.Split(';')) {
        if ($entry.TrimEnd('\') -ieq $Bin) { $count++ }
    }
    return $count
}

# Compiles a fake program to $Out. $Name is printed by --version; $Mode is fake, old, fail (exits 3
# for everything) or setupfail (setup exits 4). `setup` appends its arguments to
# %USERPROFILE%\setup.log and `wait` sleeps for ten minutes.
function Build-Fake {
    param([string]$Name, [string]$Mode, [string]$Out)
    $source = @'
using System;
using System.IO;
using System.Threading;

static class Fake
{
    static readonly string Name = "__NAME__";
    static readonly string Mode = "__MODE__";

    static int Main(string[] args)
    {
        if (Mode == "fail") { return 3; }
        string first = args.Length > 0 ? args[0] : "";
        if (first == "--version") { Console.WriteLine(Mode + "-" + Name + " 9.9.9"); return 0; }
        if (first == "setup")
        {
            string home = Environment.GetEnvironmentVariable("USERPROFILE");
            File.AppendAllText(Path.Combine(home, "setup.log"), "setup-ran " + string.Join(" ", args) + Environment.NewLine);
            return Mode == "setupfail" ? 4 : 0;
        }
        if (first == "wait") { Thread.Sleep(600000); }
        return 0;
    }
}
'@
    $file = "$Out.cs"
    [IO.File]::WriteAllText($file, $source.Replace('__NAME__', $Name).Replace('__MODE__', $Mode))
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    $result = Invoke-Program -File $csc -Arguments "/nologo /target:exe `"/out:$Out`" `"$file`""
    if ($result.Code -ne 0 -or -not (Test-Path -LiteralPath $Out)) {
        throw "The fake program $Out could not be compiled: $($result.Output)"
    }
}

# Writes zip file $Zip from $Items, each @{ Name = entry name; Source = file path, or $null for a
# folder entry; Attributes = optional ExternalAttributes value }.
function Write-Zip {
    param([object[]]$Items)
    if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
    $archive = [IO.Compression.ZipFile]::Open($Zip, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($item in $Items) {
            $entry = $archive.CreateEntry($item.Name)
            if ($item.ContainsKey('Attributes')) { $entry.ExternalAttributes = $item.Attributes }
            if ($null -ne $item.Source) {
                $bytes = [IO.File]::ReadAllBytes($item.Source)
                $stream = $entry.Open()
                try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
            }
        }
    } finally {
        $archive.Dispose()
    }
}

# Writes SHA256SUMS for the archive, one line in the normal two-column form.
function Write-Manifest {
    [IO.File]::WriteAllText($Manifest, "$(Get-Digest $Zip)  $Asset`n")
}

# Builds the fake release in $Site. $Mode is good, badrun (wi.exe does not run) or setupfail.
function Build-Release {
    param([string]$Mode = 'good')
    Initialize-Folder $Site
    [void](New-Item -ItemType Directory -Path $Rel -Force)
    $wi = $Fakes.GoodWi
    if ($Mode -eq 'badrun') { $wi = $Fakes.Fail }
    if ($Mode -eq 'setupfail') { $wi = $Fakes.SetupFailWi }
    $script:Entries = @(
        @{ Name = 'wi.exe'; Source = $wi },
        @{ Name = 'wiki-island.exe'; Source = $Fakes.GoodWikiIsland },
        @{ Name = 'LICENSE'; Source = $Fakes.License },
        @{ Name = 'README.md'; Source = $Fakes.Readme }
    )
    Write-Zip $script:Entries
    Write-Manifest
    [IO.File]::WriteAllText((Join-Path $Site 'LATEST_TAG'), "$Tag`n")
}

# Replaces the release archive with one holding $Items and rewrites SHA256SUMS to match it.
function Write-Archive {
    param([object[]]$Items)
    Write-Zip $Items
    Write-Manifest
}

# Places the older fake pair in the program folder.
function Add-OldPair {
    [void](New-Item -ItemType Directory -Path $Bin -Force)
    Copy-Item -LiteralPath $Fakes.OldWi -Destination (Join-Path $Bin 'wi.exe')
    Copy-Item -LiteralPath $Fakes.OldWikiIsland -Destination (Join-Path $Bin 'wiki-island.exe')
}

# Runs install.ps1 in a new process of this PowerShell edition. Output in $Out, exit code in $Rc.
#   -Extra     environment variables to add
#   -Keep      keep the user folders and the user Path as they are
#   -SeedOld   start with the older pair installed
#   -StartOld  also start the older wi.exe (it keeps running) before installing
#   -Piped     pipe the script text into Invoke-Expression instead of running the file
function Invoke-Install {
    param([hashtable]$Extra = @{}, [switch]$Keep, [switch]$SeedOld, [switch]$StartOld, [switch]$Piped)
    if (-not $Keep) {
        Initialize-Folder $Local
        Initialize-Folder $UserHome
        Initialize-Folder $Tmp
        Write-UserPath $SavedPath
    }
    if ($SeedOld) { Add-OldPair }
    if ($StartOld) {
        $info = New-Object System.Diagnostics.ProcessStartInfo
        $info.FileName = Join-Path $Bin 'wi.exe'
        $info.Arguments = 'wait'
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        $script:Running = [System.Diagnostics.Process]::Start($info)
    }
    $environment = @{
        LOCALAPPDATA = $Local
        USERPROFILE = $UserHome
        TEMP = $Tmp
        TMP = $Tmp
        WI_INSTALL_BASE_URL = $Url
        WI_INSTALL_DIR = $null
        WI_INSTALL_SKIP_SETUP = $null
        WI_INSTALL_ARCH = $null
        WI_INSTALL_TEST_FAIL = $null
        WI_DEBUG = '1'
    }
    foreach ($name in @($Extra.Keys)) { $environment[$name] = $Extra[$name] }
    $arguments = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$Install`""
    if ($Piped) {
        $command = "try { Get-Content -Raw -LiteralPath '$Install' | Invoke-Expression } catch { Write-Output 'caught' }; Write-Output 'still-here'"
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
        $arguments = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded"
    }
    $result = Invoke-Program -File $HostExe -Arguments $arguments -Environment $environment
    $script:Rc = $result.Code
    $script:Out = $result.Output
}

# Prints "ok <name>" when $Condition is true, else "FAIL <name>" and the installer output.
function Assert-Case {
    param([string]$Name, [scriptblock]$Condition)
    $isTrue = $false
    try { $isTrue = [bool](& $Condition) } catch { $isTrue = $false }
    if ($isTrue) {
        Write-Host "ok   $Name"
    } else {
        Write-Host "FAIL $Name"
        Write-Host ('       ' + ($Out -replace "`n", "`n       "))
        $script:Failed = 1
    }
}

# True when the installed pair is the $Kind fake pair (Good or Old).
function Test-Pair {
    param([string]$Kind)
    $wi = Join-Path $Bin 'wi.exe'
    $alias = Join-Path $Bin 'wiki-island.exe'
    if (-not (Test-Path -LiteralPath $wi -PathType Leaf) -or -not (Test-Path -LiteralPath $alias -PathType Leaf)) { return $false }
    return ((Get-Digest $wi) -eq (Get-Digest $Fakes["${Kind}Wi"])) -and ((Get-Digest $alias) -eq (Get-Digest $Fakes["${Kind}WikiIsland"]))
}

# True when no renamed, staged or temporary file of the installer is left.
function Test-NoLeftover {
    $left = @()
    if (Test-Path -LiteralPath $Bin) {
        $left += @(Get-ChildItem -LiteralPath $Bin -Force -File | Where-Object { $_.Name.StartsWith('.') })
    }
    if (Test-Path -LiteralPath $Parent) {
        $left += @(Get-ChildItem -LiteralPath $Parent -Force -Directory | Where-Object { $_.Name -like '.bin-stage-*' })
    }
    $left += @(Get-ChildItem -LiteralPath $Tmp -Force | Where-Object { $_.Name -like 'wiki-island-install-*' })
    return ($left.Count -eq 0)
}

# True when the install changed nothing: no program folder, no setup run, no leftovers.
function Test-NothingInstalled {
    return (-not (Test-Path -LiteralPath $Parent)) -and (-not (Test-Path -LiteralPath (Join-Path $UserHome 'setup.log'))) -and (Test-NoLeftover)
}

# True when the stored user Path is exactly the saved one.
function Test-PathUnchanged {
    $now = Get-UserPath
    return ($now.Exists -eq $SavedPath.Exists) -and ($now.Value -ceq $SavedPath.Value) -and ($now.Kind -eq $SavedPath.Kind)
}

# True when the output contains $Text.
function Test-Said {
    param([string]$Text)
    return $Out.Contains($Text)
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    Write-Host 'install_test.ps1 runs on Windows only.'
    exit 1
}

$SavedPath = Get-UserPath
[void](New-Item -ItemType Directory -Path $Work -Force)
try {
    $fakeDir = Join-Path $Work 'fakes'
    [void](New-Item -ItemType Directory -Path $fakeDir -Force)
    $Fakes = @{
        GoodWi = Join-Path $fakeDir 'good-wi.exe'
        GoodWikiIsland = Join-Path $fakeDir 'good-wiki-island.exe'
        OldWi = Join-Path $fakeDir 'old-wi.exe'
        OldWikiIsland = Join-Path $fakeDir 'old-wiki-island.exe'
        Fail = Join-Path $fakeDir 'fail.exe'
        SetupFailWi = Join-Path $fakeDir 'setupfail-wi.exe'
        License = Join-Path $fakeDir 'LICENSE'
        Readme = Join-Path $fakeDir 'README.md'
    }
    Build-Fake -Name 'wi' -Mode 'fake' -Out $Fakes.GoodWi
    Build-Fake -Name 'wiki-island' -Mode 'fake' -Out $Fakes.GoodWikiIsland
    Build-Fake -Name 'wi' -Mode 'old' -Out $Fakes.OldWi
    Build-Fake -Name 'wiki-island' -Mode 'old' -Out $Fakes.OldWikiIsland
    Build-Fake -Name 'wi' -Mode 'fail' -Out $Fakes.Fail
    Build-Fake -Name 'wi' -Mode 'setupfail' -Out $Fakes.SetupFailWi
    [IO.File]::WriteAllText($Fakes.License, "license`n")
    [IO.File]::WriteAllText($Fakes.Readme, "readme`n")

    # The release server from the shell test serves $Site and answers releases/latest.
    Initialize-Folder $Site
    $portFile = Join-Path $Work 'port'
    $python = Get-Command python -ErrorAction SilentlyContinue
    if ($null -eq $python) { throw 'Python 3 is needed on the path to run the release server.' }
    $serverInfo = New-Object System.Diagnostics.ProcessStartInfo
    $serverInfo.FileName = $python.Path
    $serverInfo.Arguments = "`"$(Join-Path $Here 'release_server.py')`" `"$Site`" `"$portFile`""
    $serverInfo.UseShellExecute = $false
    $serverInfo.CreateNoWindow = $true
    # The server is silent while it runs, so stderr holds only a start-up failure; read it after exit.
    $serverInfo.RedirectStandardError = $true
    $Server = [System.Diagnostics.Process]::Start($serverInfo)
    $serverWaitSteps = 600
    for ($i = 0; $i -lt $serverWaitSteps -and -not $Server.HasExited -and -not ((Test-Path -LiteralPath $portFile) -and (Get-Item -LiteralPath $portFile).Length -gt 0); $i++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $portFile) -or (Get-Item -LiteralPath $portFile).Length -eq 0) {
        if ($Server.HasExited) {
            $serverError = $Server.StandardError.ReadToEnd().Trim()
            [IO.File]::WriteAllText((Join-Path $Work 'release_server.err'), $serverError)
            throw "The release server did not start: it exited with code $($Server.ExitCode). stderr: $serverError"
        }
        throw 'The release server did not start within 60 seconds.'
    }
    $Url = "http://127.0.0.1:$([IO.File]::ReadAllText($portFile).Trim())"

    # 1. Verified install.
    Build-Release
    Invoke-Install
    Assert-Case 'verified install exits 0' { $Rc -eq 0 }
    Assert-Case 'verified install puts the release pair in place' { Test-Pair 'Good' }
    Assert-Case 'verified install runs wi setup from the program folder' { (Get-Content -Raw -LiteralPath (Join-Path $UserHome 'setup.log')).Contains('setup-ran setup') }
    Assert-Case 'verified install adds the folder to the user Path' { (Get-PathEntryCount) -eq 1 }
    Assert-Case 'verified install says it is done' { Test-Said 'Done. Nothing else to do' }
    Assert-Case 'verified install leaves no staging files' { Test-NoLeftover }

    # 2. A second run adds the Path entry once.
    Invoke-Install -Keep
    Assert-Case 'second run exits 0' { $Rc -eq 0 }
    Assert-Case 'second run keeps one Path entry' { (Get-PathEntryCount) -eq 1 }
    Assert-Case 'second run says the folder is already on the Path' { Test-Said 'is on your user Path' }

    # 3. A REG_EXPAND_SZ user Path with %USERPROFILE% keeps its kind and its text.
    $expand = '%USERPROFILE%\wi-test-tools;C:\wi-test-other'
    Initialize-Folder $Local
    Initialize-Folder $UserHome
    Initialize-Folder $Tmp
    Write-UserPath @{ Exists = $true; Kind = [Microsoft.Win32.RegistryValueKind]::ExpandString; Value = $expand }
    Invoke-Install -Keep
    $stored = Get-UserPath
    Assert-Case 'expandable Path install exits 0' { $Rc -eq 0 }
    Assert-Case 'expandable Path keeps REG_EXPAND_SZ' { $stored.Kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString }
    Assert-Case 'expandable Path keeps %USERPROFILE% unexpanded and appends the folder' { $stored.Value -ceq "$expand;$Bin" }

    # 4. A REG_SZ user Path stays REG_SZ.
    Initialize-Folder $Local
    Write-UserPath @{ Exists = $true; Kind = [Microsoft.Win32.RegistryValueKind]::String; Value = 'C:\wi-test-plain;' }
    Invoke-Install -Keep
    $stored = Get-UserPath
    Assert-Case 'plain Path keeps REG_SZ' { $stored.Kind -eq [Microsoft.Win32.RegistryValueKind]::String }
    Assert-Case 'plain Path ending in ; gains one entry' { $stored.Value -ceq "C:\wi-test-plain;$Bin" }

    # 5. No user Path value at all: it is created as REG_EXPAND_SZ.
    Initialize-Folder $Local
    Write-UserPath @{ Exists = $false; Kind = $null; Value = $null }
    Invoke-Install -Keep
    $stored = Get-UserPath
    Assert-Case 'missing Path is created as REG_EXPAND_SZ' { $stored.Exists -and $stored.Kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString }
    Assert-Case 'missing Path is created holding only the folder' { $stored.Value -ceq $Bin }

    # 6. Checksum mismatch over an older pair: non-zero, nothing changed.
    Build-Release
    [IO.File]::WriteAllText($Manifest, ('0' * 64) + "  $Asset`n")
    Invoke-Install -SeedOld
    Assert-Case 'checksum mismatch exits non-zero' { $Rc -ne 0 }
    Assert-Case 'checksum mismatch keeps the older pair' { Test-Pair 'Old' }
    Assert-Case 'checksum mismatch leaves the user Path alone' { Test-PathUnchanged }
    Assert-Case 'checksum mismatch runs no setup and leaves nothing behind' { (-not (Test-Path -LiteralPath (Join-Path $UserHome 'setup.log'))) -and (Test-NoLeftover) }
    Assert-Case 'checksum mismatch says so in plain words' { Test-Said 'does not match its checksum' }

    # 7. Checksum mismatch on a fresh computer: nothing installed, no folder created.
    Invoke-Install
    Assert-Case 'fresh checksum mismatch exits non-zero' { $Rc -ne 0 }
    Assert-Case 'fresh checksum mismatch installs nothing' { Test-NothingInstalled }

    # 8. A checksum list without the asset.
    Build-Release
    [IO.File]::WriteAllText($Manifest, "abc  other-file.zip`n")
    Invoke-Install
    Assert-Case 'list without the asset exits non-zero' { $Rc -ne 0 }
    Assert-Case 'list without the asset installs nothing' { Test-NothingInstalled }
    Assert-Case 'list without the asset says the list is damaged' { Test-Said 'checksum list is damaged' }

    # 9. Two matching lines.
    Build-Release
    $line = "$(Get-Digest $Zip)  $Asset`n"
    [IO.File]::WriteAllText($Manifest, $line + $line)
    Invoke-Install
    Assert-Case 'two matching lines exit non-zero' { $Rc -ne 0 }
    Assert-Case 'two matching lines install nothing' { Test-NothingInstalled }

    # 10. A matching line with an extra field.
    Build-Release
    [IO.File]::WriteAllText($Manifest, "$(Get-Digest $Zip)  $Asset junk`n")
    Invoke-Install
    Assert-Case 'extra field exits non-zero' { $Rc -ne 0 }
    Assert-Case 'extra field installs nothing' { Test-NothingInstalled }

    # 11. CRLF line endings are accepted.
    Build-Release
    [IO.File]::WriteAllText($Manifest, "$(Get-Digest $Zip)  $Asset`r`n")
    Invoke-Install
    Assert-Case 'CRLF list installs' { $Rc -eq 0 }
    Assert-Case 'CRLF list installs the release pair' { Test-Pair 'Good' }

    # 12. An upper-case digest with the binary-mode '*' marker is accepted.
    Build-Release
    [IO.File]::WriteAllText($Manifest, "$((Get-Digest $Zip).ToUpperInvariant()) *$Asset`n")
    Invoke-Install
    Assert-Case 'upper-case digest installs' { $Rc -eq 0 }
    Assert-Case 'upper-case digest installs the release pair' { Test-Pair 'Good' }

    # 13. The release has no Windows archive.
    Build-Release
    Remove-Item -LiteralPath $Zip -Force
    Invoke-Install
    Assert-Case 'missing asset exits non-zero' { $Rc -ne 0 }
    Assert-Case 'missing asset installs nothing' { Test-NothingInstalled }
    Assert-Case 'missing asset says the release has no program for Windows' { Test-Said 'no program for Windows' }

    # 14. The release has no checksum list.
    Build-Release
    Remove-Item -LiteralPath $Manifest -Force
    Invoke-Install
    Assert-Case 'missing checksum list exits non-zero' { $Rc -ne 0 }
    Assert-Case 'missing checksum list installs nothing' { Test-NothingInstalled }
    Assert-Case 'missing checksum list says so' { Test-Said 'no checksum list' }

    # 15. No release at all: releases/latest answers 404.
    Build-Release
    Remove-Item -LiteralPath (Join-Path $Site 'LATEST_TAG') -Force
    Invoke-Install
    Assert-Case 'no release exits non-zero' { $Rc -ne 0 }
    Assert-Case 'no release installs nothing' { Test-NothingInstalled }
    Assert-Case 'no release says nothing is published yet' { Test-Said 'published yet' }
    Assert-Case 'no release downloads nothing' { -not (Test-Said 'Downloading') }

    # 16. No published release: releases/latest redirects to /releases, as GitHub does.
    Build-Release
    Remove-Item -LiteralPath (Join-Path $Site 'LATEST_TAG') -Force
    [IO.File]::WriteAllText((Join-Path $Site 'LATEST_REDIRECT'), "/releases`n")
    Invoke-Install
    Assert-Case 'redirect to /releases exits non-zero' { $Rc -ne 0 }
    Assert-Case 'redirect to /releases installs nothing' { Test-NothingInstalled }
    Assert-Case 'redirect to /releases says nothing is published yet' { Test-Said 'published yet' }

    # 17. The server cannot be reached.
    Build-Release
    Invoke-Install -Extra @{ WI_INSTALL_BASE_URL = 'http://127.0.0.1:1' }
    Assert-Case 'unreachable server exits non-zero' { $Rc -ne 0 }
    Assert-Case 'unreachable server installs nothing' { Test-NothingInstalled }
    Assert-Case 'unreachable server says GitHub could not be reached' { Test-Said 'could not be reached' }

    # 18. An archive with a '..' entry: stop before writing anything.
    Build-Release
    Write-Archive ($Entries + @(@{ Name = '../escaped'; Source = $Fakes.License }))
    Invoke-Install
    Assert-Case "'..' entry exits non-zero" { $Rc -ne 0 }
    Assert-Case "'..' entry installs nothing" { Test-NothingInstalled }
    Assert-Case "'..' entry writes nothing outside" { @(Get-ChildItem -LiteralPath $Work -Recurse -Force -Filter 'escaped').Count -eq 0 }
    Assert-Case "'..' entry says so in a plain sentence" { Test-Said 'unexpected files' }

    # 19. An archive with a drive-qualified entry.
    Build-Release
    Write-Archive ($Entries + @(@{ Name = 'C:/escaped'; Source = $Fakes.License }))
    Invoke-Install
    Assert-Case 'drive-qualified entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'drive-qualified entry installs nothing' { Test-NothingInstalled }

    # 20. An archive with an extra entry.
    Build-Release
    Write-Archive ($Entries + @(@{ Name = 'unexpected'; Source = $Fakes.License }))
    Invoke-Install
    Assert-Case 'extra entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'extra entry installs nothing' { Test-NothingInstalled }

    # 21. An archive with the same entry twice.
    Build-Release
    Write-Archive ($Entries + @(@{ Name = 'wi.exe'; Source = $Fakes.GoodWi }))
    Invoke-Install
    Assert-Case 'duplicate entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'duplicate entry installs nothing' { Test-NothingInstalled }

    # 22. An archive with a folder where wi.exe belongs.
    Build-Release
    Write-Archive @(
        @{ Name = 'wi.exe/'; Source = $null },
        @{ Name = 'wiki-island.exe'; Source = $Fakes.GoodWikiIsland },
        @{ Name = 'LICENSE'; Source = $Fakes.License },
        @{ Name = 'README.md'; Source = $Fakes.Readme }
    )
    Invoke-Install
    Assert-Case 'folder entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'folder entry installs nothing' { Test-NothingInstalled }

    # 23. An archive without README.md.
    Build-Release
    Write-Archive @($Entries[0], $Entries[1], $Entries[2])
    Invoke-Install
    Assert-Case 'missing entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'missing entry installs nothing' { Test-NothingInstalled }

    # 24. A folder sits where wiki-island.exe belongs: stop before replacing anything.
    Build-Release
    Initialize-Folder $Local
    Initialize-Folder $UserHome
    Initialize-Folder $Tmp
    Write-UserPath $SavedPath
    [void](New-Item -ItemType Directory -Path (Join-Path $Bin 'wiki-island.exe') -Force)
    Copy-Item -LiteralPath $Fakes.OldWi -Destination (Join-Path $Bin 'wi.exe')
    Invoke-Install -Keep
    Assert-Case 'folder destination exits non-zero' { $Rc -ne 0 }
    Assert-Case 'folder destination keeps the older wi.exe' { (Get-Digest (Join-Path $Bin 'wi.exe')) -eq (Get-Digest $Fakes.OldWi) }
    Assert-Case 'folder destination leaves the folder empty' { @(Get-ChildItem -LiteralPath (Join-Path $Bin 'wiki-island.exe') -Force).Count -eq 0 }
    Assert-Case 'folder destination says it is in the way' { Test-Said 'is in the way' }

    # 25. The downloaded wi.exe does not run: the older pair is kept.
    Build-Release -Mode 'badrun'
    Invoke-Install -SeedOld
    Assert-Case 'program that does not run exits non-zero' { $Rc -ne 0 }
    Assert-Case 'program that does not run keeps the older pair' { Test-Pair 'Old' }
    Assert-Case 'program that does not run leaves nothing behind' { Test-NoLeftover }
    Assert-Case 'program that does not run says so' { Test-Said 'does not run on this computer' }

    # 26. A verified install over an older pair replaces both.
    Build-Release
    Invoke-Install -SeedOld
    Assert-Case 'upgrade exits 0' { $Rc -eq 0 }
    Assert-Case 'upgrade replaces both programs' { Test-Pair 'Good' }
    Assert-Case 'upgrade leaves no staging files' { Test-NoLeftover }

    # 27. The older wi.exe is running: it is renamed aside and the install finishes. The next run
    # removes the renamed copy once it has stopped.
    Build-Release
    Invoke-Install -SeedOld -StartOld
    Assert-Case 'upgrade over a running wi.exe exits 0' { $Rc -eq 0 }
    Assert-Case 'upgrade over a running wi.exe installs the release pair' { Test-Pair 'Good' }
    Assert-Case 'the running older wi.exe kept running' { -not $Running.HasExited }
    $Running.Kill()
    $Running.WaitForExit()
    $Running = $null
    Invoke-Install -Keep
    Assert-Case 'the next run exits 0' { $Rc -eq 0 }
    Assert-Case 'the next run removes the renamed older wi.exe' { Test-NoLeftover }

    # 28. Moving the new wiki-island.exe in fails: the older pair is restored.
    Build-Release
    Invoke-Install -SeedOld -Extra @{ WI_INSTALL_TEST_FAIL = 'place:wiki-island.exe' }
    Assert-Case 'failed placement exits non-zero' { $Rc -ne 0 }
    Assert-Case 'failed placement restores the older pair' { Test-Pair 'Old' }
    Assert-Case 'failed placement leaves nothing behind' { Test-NoLeftover }
    Assert-Case 'failed placement says the previous programs were kept' { Test-Said 'previous ones were kept' }
    Assert-Case 'failed placement leaves the user Path alone' { Test-PathUnchanged }

    # 29. The restore fails too: the installer says the installation is incomplete.
    Build-Release
    Invoke-Install -SeedOld -Extra @{ WI_INSTALL_TEST_FAIL = 'place:wiki-island.exe,restore:wi.exe' }
    Assert-Case 'failed restore exits non-zero' { $Rc -ne 0 }
    Assert-Case 'failed restore says the installation is incomplete' { Test-Said 'incomplete' }
    Assert-Case 'failed restore does not claim the previous programs were kept' { -not (Test-Said 'were kept') }
    Assert-Case 'failed restore leaves both names runnable' {
        ((Invoke-Program -File (Join-Path $Bin 'wi.exe') -Arguments '--version').Code -eq 0) -and
        ((Invoke-Program -File (Join-Path $Bin 'wiki-island.exe') -Arguments '--version').Code -eq 0)
    }

    # 30. An unsupported processor architecture: stop before any download or write.
    Build-Release
    Invoke-Install -Extra @{ WI_INSTALL_ARCH = 'ARM64' }
    Assert-Case 'unsupported architecture exits non-zero' { $Rc -ne 0 }
    Assert-Case 'unsupported architecture installs nothing' { Test-NothingInstalled }
    Assert-Case 'unsupported architecture downloads nothing' { -not (Test-Said 'Downloading') }
    Assert-Case 'unsupported architecture says only x86_64 is supported' { Test-Said 'only x86_64' }

    # 31. wi setup fails: non-zero, and the message says the programs are installed.
    Build-Release -Mode 'setupfail'
    Invoke-Install
    Assert-Case 'failing setup exits non-zero' { $Rc -ne 0 }
    Assert-Case 'failing setup says wi setup did not finish' { Test-Said 'wi setup did not finish' }

    # 32. WI_INSTALL_DIR replaces the program folder.
    Build-Release
    $custom = Join-Path $Work 'custom\bin'
    Invoke-Install -Extra @{ WI_INSTALL_DIR = $custom }
    Assert-Case 'WI_INSTALL_DIR install exits 0' { $Rc -eq 0 }
    Assert-Case 'WI_INSTALL_DIR receives the programs' { Test-Path -LiteralPath (Join-Path $custom 'wi.exe') -PathType Leaf }

    # 33. Piped into Invoke-Expression, as the one-line command does: it installs, and a failure
    # is a catchable error that leaves the calling PowerShell running.
    Build-Release
    Invoke-Install -Piped
    Assert-Case 'piped install exits 0' { $Rc -eq 0 }
    Assert-Case 'piped install puts the release pair in place' { Test-Pair 'Good' }
    Assert-Case 'piped install returns to the caller' { (Test-Said 'still-here') -and -not (Test-Said 'caught') }
    [IO.File]::WriteAllText($Manifest, ('0' * 64) + "  $Asset`n")
    Invoke-Install -Piped
    Assert-Case 'piped failure is a catchable error' { (Test-Said 'caught') -and (Test-Said 'still-here') }
    Assert-Case 'piped failure installs nothing' { Test-NothingInstalled }
    Assert-Case 'piped failure says why in plain words' { Test-Said 'does not match its checksum' }

    # 34. The install folder is a junction to another folder: nothing is read, written or removed
    # there. The junction is deleted before the next cleanup so no recursive delete follows it.
    Build-Release
    $linkTarget = Join-Path $Work 'link-target'
    Initialize-Folder $linkTarget
    [IO.File]::WriteAllText((Join-Path $linkTarget '.wi.exe.old-keep'), "keep`n")
    Initialize-Folder $Local
    Initialize-Folder $UserHome
    Initialize-Folder $Tmp
    Write-UserPath $SavedPath
    [void](New-Item -ItemType Directory -Path $Parent -Force)
    [void](New-Item -ItemType Junction -Path $Bin -Target $linkTarget)
    Invoke-Install -Keep
    [IO.Directory]::Delete($Bin)
    Assert-Case 'linked install folder exits non-zero' { $Rc -ne 0 }
    Assert-Case 'linked install folder says it is a link' { Test-Said 'is a link to another place' }
    Assert-Case 'linked install folder keeps the file in the target' { Test-Path -LiteralPath (Join-Path $linkTarget '.wi.exe.old-keep') -PathType Leaf }
    Assert-Case 'linked install folder writes nothing into the target' { @(Get-ChildItem -LiteralPath $linkTarget -Force).Count -eq 1 }
    Assert-Case 'linked install folder runs no setup and leaves the user Path alone' { (-not (Test-Path -LiteralPath (Join-Path $UserHome 'setup.log'))) -and (Test-PathUnchanged) }

    # 35. The parent of the install folder is a junction.
    Build-Release
    $linkParent = Join-Path $Work 'link-parent'
    Initialize-Folder $linkParent
    [void](New-Item -ItemType Directory -Path (Join-Path $linkParent '.bin-stage-keep'))
    Initialize-Folder $Local
    Initialize-Folder $UserHome
    Initialize-Folder $Tmp
    Write-UserPath $SavedPath
    [void](New-Item -ItemType Directory -Path (Join-Path $Local 'Programs') -Force)
    [void](New-Item -ItemType Junction -Path $Parent -Target $linkParent)
    Invoke-Install -Keep
    [IO.Directory]::Delete($Parent)
    Assert-Case 'linked parent folder exits non-zero' { $Rc -ne 0 }
    Assert-Case 'linked parent folder says it is a link' { Test-Said 'is a link to another place' }
    Assert-Case 'linked parent folder keeps the folder in the target' { Test-Path -LiteralPath (Join-Path $linkParent '.bin-stage-keep') -PathType Container }
    Assert-Case 'linked parent folder writes nothing into the target' { @(Get-ChildItem -LiteralPath $linkParent -Force).Count -eq 1 }

    # 36. An install folder name with ';' is not added to the Path; the programs are installed.
    Build-Release
    $semi = Join-Path $Work 'semi;dir\bin'
    Invoke-Install -Extra @{ WI_INSTALL_DIR = $semi }
    Assert-Case "install folder with ';' exits 0" { $Rc -eq 0 }
    Assert-Case "install folder with ';' receives the programs" { Test-Path -LiteralPath (Join-Path $semi 'wi.exe') -PathType Leaf }
    Assert-Case "install folder with ';' leaves the user Path as it was" { Test-PathUnchanged }
    Assert-Case "install folder with ';' says it cannot be added to the Path" { Test-Said 'cannot be added to your user Path' }
    Assert-Case "install folder with ';' still runs wi setup" { (Get-Content -Raw -LiteralPath (Join-Path $UserHome 'setup.log')).Contains('setup-ran setup') }

    # 37. An install folder name with '%' is not added to the Path either.
    Build-Release
    $percent = Join-Path $Work 'pct%TEMP%dir\bin'
    Invoke-Install -Extra @{ WI_INSTALL_DIR = $percent }
    Assert-Case "install folder with '%' exits 0" { $Rc -eq 0 }
    Assert-Case "install folder with '%' receives the programs" { Test-Path -LiteralPath (Join-Path $percent 'wi.exe') -PathType Leaf }
    Assert-Case "install folder with '%' leaves the user Path as it was" { Test-PathUnchanged }
    Assert-Case "install folder with '%' says it cannot be added to the Path" { Test-Said 'cannot be added to your user Path' }

    # 38. An archive entry that carries directory attributes is refused, whatever its name.
    Build-Release
    Write-Archive @(
        $Entries[0], $Entries[1], $Entries[2],
        @{ Name = 'README.md'; Source = $Fakes.Readme; Attributes = 0x10 }
    )
    Invoke-Install
    Assert-Case 'directory-attribute entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'directory-attribute entry installs nothing' { Test-NothingInstalled }
    Assert-Case 'directory-attribute entry says so in a plain sentence' { Test-Said 'unexpected files' }

    # 39. An archive entry typed as a symbolic link in its Unix mode bits is refused.
    Build-Release
    $symlinkMode = [BitConverter]::ToInt32([BitConverter]::GetBytes([uint32]2717908992), 0)
    Write-Archive @(
        $Entries[0], $Entries[1], $Entries[2],
        @{ Name = 'README.md'; Source = $Fakes.Readme; Attributes = $symlinkMode }
    )
    Invoke-Install
    Assert-Case 'symbolic-link entry exits non-zero' { $Rc -ne 0 }
    Assert-Case 'symbolic-link entry installs nothing' { Test-NothingInstalled }
} finally {
    if ($null -ne $Running -and -not $Running.HasExited) { $Running.Kill() }
    if ($null -ne $Server -and -not $Server.HasExited) { $Server.Kill() }
    Write-UserPath $SavedPath
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($Failed -eq 0) { Write-Host 'all install tests passed' }
exit $Failed
