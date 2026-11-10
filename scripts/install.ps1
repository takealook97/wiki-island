# wiki-island installer for Windows. Safe to run again. Needs no administrator rights.
#
# One-line use in PowerShell:
#   irm https://raw.githubusercontent.com/takealook97/wiki-island/main/scripts/install.ps1 | iex
#
# It installs the prebuilt wi.exe and wiki-island.exe from the latest GitHub release after
# checking their SHA-256 checksum, adds their folder to the user Path once and runs `wi setup`.
# It runs on Windows PowerShell 5.1 and PowerShell 7 and never builds from source.
#
# Environment variables:
#   WI_INSTALL_BASE_URL    Repository URL to download from (default https://github.com/takealook97/wiki-island).
#   WI_INSTALL_DIR         Tests only: the program folder (default %LOCALAPPDATA%\Programs\wiki-island\bin).
#   WI_INSTALL_SKIP_SETUP  Tests only: set to 1 to skip `wi setup`.
#   WI_INSTALL_ARCH        Tests only: replaces the detected processor architecture.
#   WI_INSTALL_TEST_FAIL   Tests only: comma-separated replacement steps that fail on purpose,
#                          each `aside:<program>`, `place:<program>` or `restore:<program>`.
#   WI_INSTALL_MAX_ASSET_BYTES  Tests only: a smaller size limit for the release archive, in bytes;
#                          a value at or above the normal limit is ignored.
#   WI_DEBUG               Set to 1 to print the details of an unexpected error.

$WiRepoUrl = 'https://github.com/takealook97/wiki-island'
$WiIssues = "$WiRepoUrl/issues"
$WiBuildDocs = "$WiRepoUrl#windows"
$WiPrograms = @('wi.exe', 'wiki-island.exe')
$WiArchiveEntries = @('wi.exe', 'wiki-island.exe', 'LICENSE', 'README.md')
$WiKeep = 'check that your user folder has free space and can be written to, then run this installer again.'
# Download size limits in bytes: 256 MiB for the release archive, 64 KiB for SHA256SUMS. They are
# unmeasured policy values well above the current release (an archive is about 18 MB).
$WiMaxAssetBytes = [long]268435456
$WiMaxSumsBytes = [long]65536

# Writes one line for the person running the installer.
function Write-WiLine {
    param([string]$Text)
    Write-Host $Text
}

# Stops the installer with one plain sentence. $Text is the whole sentence when $Whole is set,
# otherwise it follows "wiki-island could not be installed: ".
function Invoke-WiFailure {
    param([string]$Text, [switch]$Whole)
    $message = $Text
    if (-not $Whole) { $message = "wiki-island could not be installed: $Text" }
    $failure = New-Object System.Exception $message
    $failure.Data['WiPlain'] = $true
    throw $failure
}

# True when the environment variable $Name is exactly "1".
function Test-WiFlag {
    param([string]$Name)
    return ([Environment]::GetEnvironmentVariable($Name) -eq '1')
}

# Throws when the test hook WI_INSTALL_TEST_FAIL names replacement step $Step.
function Invoke-WiTestHook {
    param([string]$Step)
    $hook = [Environment]::GetEnvironmentVariable('WI_INSTALL_TEST_FAIL')
    if ([string]::IsNullOrEmpty($hook)) { return }
    foreach ($item in $hook.Split(',')) {
        if ($item.Trim() -ceq $Step) { throw (New-Object System.IO.IOException "injected failure at $Step") }
    }
}

# Returns the processor architecture of Windows itself, not of this PowerShell process.
function Get-WiArchitecture {
    $forced = [Environment]::GetEnvironmentVariable('WI_INSTALL_ARCH')
    if (-not [string]::IsNullOrEmpty($forced)) { return $forced }
    try {
        $key = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
        $native = (Get-ItemProperty -LiteralPath $key -Name 'PROCESSOR_ARCHITECTURE').PROCESSOR_ARCHITECTURE
        if (-not [string]::IsNullOrEmpty($native)) { return $native }
    } catch {
        Write-Verbose 'The system processor architecture could not be read from the registry.'
    }
    $wow = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITEW6432')
    if (-not [string]::IsNullOrEmpty($wow)) { return $wow }
    return [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE')
}

# Returns the program folder.
function Get-WiInstallDir {
    $dir = [Environment]::GetEnvironmentVariable('WI_INSTALL_DIR')
    if ([string]::IsNullOrEmpty($dir)) {
        $local = [Environment]::GetEnvironmentVariable('LOCALAPPDATA')
        if ([string]::IsNullOrEmpty($local) -or -not [IO.Path]::IsPathRooted($local)) {
            Invoke-WiFailure 'the local app data folder (LOCALAPPDATA) is not set, so nothing was installed; sign in to Windows with your own account, then run this installer again.'
        }
        $dir = Join-Path $local 'Programs\wiki-island\bin'
    }
    if (-not [IO.Path]::IsPathRooted($dir)) {
        Invoke-WiFailure 'the program folder is not a full path, so nothing was installed; set WI_INSTALL_DIR to a full path or leave it unset, then run this installer again.'
    }
    return [IO.Path]::GetFullPath($dir).TrimEnd('\')
}

# Returns the size limit for the release archive: $WiMaxAssetBytes, or the smaller test value in
# WI_INSTALL_MAX_ASSET_BYTES.
function Get-WiAssetLimit {
    $forced = [Environment]::GetEnvironmentVariable('WI_INSTALL_MAX_ASSET_BYTES')
    $value = [long]0
    if ([long]::TryParse([string]$forced, [ref]$value) -and $value -gt 0 -and $value -lt $WiMaxAssetBytes) { return $value }
    return $WiMaxAssetBytes
}

# True when $Uri uses https, or http when WI_INSTALL_BASE_URL is set (tests serve over http).
function Test-WiScheme {
    param([Uri]$Uri)
    if ($Uri.Scheme -eq 'https') { return $true }
    $override = [Environment]::GetEnvironmentVariable('WI_INSTALL_BASE_URL')
    return ($Uri.Scheme -eq 'http' -and -not [string]::IsNullOrEmpty($override))
}

# Returns the HTTP response carried by error record $ErrorRecord, or $null when the request
# failed before any answer (no connection, timeout, refused redirect).
function Get-WiErrorResponse {
    param($ErrorRecord)
    $inner = $ErrorRecord.Exception
    while ($null -ne $inner) {
        if ($inner -is [System.Net.WebException]) { return $inner.Response }
        $inner = $inner.InnerException
    }
    return $null
}

# Creates a GET request for $Url.
function Get-WiRequest {
    param([string]$Url, [bool]$FollowRedirects)
    $request = [System.Net.WebRequest]::Create($Url)
    $request.Method = 'GET'
    $request.AllowAutoRedirect = $FollowRedirects
    $request.Timeout = 60000
    $request.ReadWriteTimeout = 300000
    $request.UserAgent = 'wiki-island-installer'
    return $request
}

# Asks $Base/releases/latest without following its redirect. Returns the tag, or one of
# 'unreachable', 'none' (no published release) and 'badname'.
function Get-WiLatestTag {
    param([string]$Base)
    $response = $null
    try {
        $response = (Get-WiRequest "$Base/releases/latest" $false).GetResponse()
    } catch {
        $response = Get-WiErrorResponse $_
        if ($null -eq $response) { return 'unreachable' }
    }
    try {
        $status = [int]$response.StatusCode
        $location = [string]$response.Headers['Location']
    } finally {
        $response.Close()
    }
    if (@(301, 302, 303, 307, 308) -notcontains $status) { return 'none' }
    $found = [regex]::Match($location, '/releases/tag/([^/?#]*)$')
    if (-not $found.Success) { return 'none' }
    $tag = $found.Groups[1].Value
    if ($tag -cnotmatch '^v[A-Za-z0-9._-]+$') { return 'badname' }
    return $tag
}

# Downloads $Url to $OutFile, at most $MaxBytes bytes. Returns 'ok', 'missing' (HTTP 404),
# 'toolarge' (the stated size or the bytes received exceed $MaxBytes; the partial file is removed)
# or 'error' (anything else). Bytes are counted while they arrive, so a server that states no size
# or a false one cannot get past the limit.
function Get-WiDownload {
    param([string]$Url, [string]$OutFile, [long]$MaxBytes)
    try {
        $response = (Get-WiRequest $Url $true).GetResponse()
    } catch {
        $answer = Get-WiErrorResponse $_
        if ($null -eq $answer) { return 'error' }
        $status = [int]$answer.StatusCode
        $answer.Close()
        if ($status -eq 404) { return 'missing' }
        return 'error'
    }
    try {
        if ([int]$response.StatusCode -ne 200 -or -not (Test-WiScheme $response.ResponseUri)) { return 'error' }
        if ($response.ContentLength -gt $MaxBytes) { return 'toolarge' }
        $source = $response.GetResponseStream()
        $target = [IO.File]::Open($OutFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
        $isTooLarge = $false
        try {
            $buffer = New-Object byte[] 65536
            $received = [long]0
            $count = $source.Read($buffer, 0, $buffer.Length)
            while ($count -gt 0) {
                $received += $count
                if ($received -gt $MaxBytes) {
                    $isTooLarge = $true
                    break
                }
                $target.Write($buffer, 0, $count)
                $count = $source.Read($buffer, 0, $buffer.Length)
            }
        } finally {
            $target.Dispose()
            $source.Dispose()
        }
        if ($isTooLarge) {
            Remove-Item -LiteralPath $OutFile -Force -ErrorAction SilentlyContinue
            return 'toolarge'
        }
    } catch {
        return 'error'
    } finally {
        $response.Close()
    }
    return 'ok'
}

# Returns the lower-case digest for asset $Asset from checksum list $ManifestPath. Exactly one line
# may name the asset (optionally with the binary-mode '*'); it must have two fields and a 64-digit
# hex digest. Lines may end in CRLF.
function Get-WiExpectedDigest {
    param([string]$ManifestPath, [string]$Asset)
    $bad = "the published checksum list is damaged, so nothing was installed. Run this installer again later, or report it at $WiIssues."
    $found = @()
    foreach ($line in [IO.File]::ReadAllText($ManifestPath).Split("`n")) {
        $text = $line
        if ($text.EndsWith("`r")) { $text = $text.Substring(0, $text.Length - 1) }
        $fields = @([regex]::Split($text, '[ \t]+') | Where-Object { $_.Length -gt 0 })
        if ($fields.Count -lt 2) { continue }
        $name = $fields[1]
        if ($name.StartsWith('*')) { $name = $name.Substring(1) }
        if ($name -ceq $Asset) { $found += , $fields }
    }
    if ($found.Count -ne 1) { Invoke-WiFailure $bad }
    $fields = $found[0]
    if ($fields.Count -ne 2) { Invoke-WiFailure $bad }
    $digest = $fields[0].ToLowerInvariant()
    if ($digest -cnotmatch '^[0-9a-f]{64}$') { Invoke-WiFailure $bad }
    return $digest
}

# Stops unless archive $Archive holds exactly the release entries as files (each optionally
# prefixed by ./) and nothing else. Nothing is extracted here.
function Test-WiArchive {
    param($Archive)
    $unexpected = "the downloaded release contains unexpected files, so nothing was installed. Report it at $WiIssues, then run this installer again later."
    $seen = @()
    foreach ($entry in $Archive.Entries) {
        $name = $entry.FullName
        if ($name.StartsWith('./')) { $name = $name.Substring(2) }
        if ($WiArchiveEntries -cnotcontains $name) { Invoke-WiFailure $unexpected }
        if ($seen -ccontains $name) { Invoke-WiFailure $unexpected }
        if ($entry.Name -cne $name) { Invoke-WiFailure $unexpected }
        # Only a plain file may be extracted: reject the DOS directory bit and any Unix file type
        # (high 16 bits of the attributes) other than a regular file.
        $attributes = $entry.ExternalAttributes
        $unixType = ($attributes -shr 16) -band 0xF000
        if (($attributes -band 0x10) -ne 0 -or ($unixType -ne 0 -and $unixType -ne 0x8000)) { Invoke-WiFailure $unexpected }
        $seen += $name
    }
    if ($seen.Count -ne $WiArchiveEntries.Count) { Invoke-WiFailure $unexpected }
}

# Writes the programs of zip file $ZipPath into the new folder $Stage, one entry at a time, after
# checking the whole entry list.
function Expand-WiPair {
    param([string]$ZipPath, [string]$Stage)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $unpack = "the downloaded program could not be unpacked, so nothing was installed. Run this installer again later, or report it at $WiIssues."
    try {
        $archive = [IO.Compression.ZipFile]::OpenRead($ZipPath)
    } catch {
        Invoke-WiFailure $unpack
    }
    try {
        Test-WiArchive $archive
        try {
            [void][IO.Directory]::CreateDirectory($Stage)
        } catch {
            Invoke-WiFailure "the new programs could not be copied into place, so nothing was installed; $WiKeep"
        }
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName
            if ($name.StartsWith('./')) { $name = $name.Substring(2) }
            if ($WiPrograms -cnotcontains $name) { continue }
            try {
                $source = $entry.Open()
                try {
                    $target = [IO.File]::Open((Join-Path $Stage $name), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
                    try { $source.CopyTo($target) } finally { $target.Dispose() }
                } finally {
                    $source.Dispose()
                }
            } catch {
                Invoke-WiFailure $unpack
            }
        }
    } finally {
        $archive.Dispose()
    }
}

# Runs program $Path with arguments $Arguments, its output discarded. Returns its exit code, or
# -1 when it cannot be started or does not finish within $Seconds.
function Invoke-WiQuiet {
    param([string]$Path, [string]$Arguments, [int]$Seconds)
    try {
        $info = New-Object System.Diagnostics.ProcessStartInfo
        $info.FileName = $Path
        $info.Arguments = $Arguments
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        $info.RedirectStandardOutput = $true
        $info.RedirectStandardError = $true
        $process = [System.Diagnostics.Process]::Start($info)
        $null = $process.StandardOutput.ReadToEndAsync()
        $null = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($Seconds * 1000)) {
            try { $process.Kill() } catch { Write-Verbose 'The program had already ended.' }
            return -1
        }
        return $process.ExitCode
    } catch {
        return -1
    }
}

# True when $Path exists and is a reparse point (a junction or a symbolic link).
function Test-WiLink {
    param([string]$Path)
    try {
        $attributes = [IO.File]::GetAttributes($Path)
    } catch {
        return $false
    }
    return (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)
}

# Stops unless the install folder $Dir and every existing folder above it, up to the drive root,
# are real folders, not links to another place (a symbolic link or junction). Nothing is read,
# written or removed before this passes.
function Assert-WiNotLinked {
    param([string]$Dir)
    $path = $Dir
    while (-not [string]::IsNullOrEmpty($path)) {
        if (Test-WiLink $path) {
            Invoke-WiFailure "the folder $path is a link to another place, so nothing was installed; remove the link or set WI_INSTALL_DIR to a normal folder, then run this installer again."
        }
        $path = [IO.Path]::GetDirectoryName($path)
    }
}

# Removes files and folders left by an earlier run that could not remove them (for example a
# renamed wi.exe that was still running). Best effort: what cannot be removed stays for next time.
function Clear-WiLeftover {
    param([string]$Dir)
    $parent = Split-Path -Parent $Dir
    $leaf = Split-Path -Leaf $Dir
    if (Test-Path -LiteralPath $Dir -PathType Container) {
        foreach ($item in @(Get-ChildItem -LiteralPath $Dir -Force -File)) {
            if ($item.Name -like '.*.old-*' -or $item.Name -like '.*.bad-*') {
                Remove-Item -LiteralPath $item.FullName -Force -ErrorAction SilentlyContinue
            }
        }
    }
    if (Test-Path -LiteralPath $parent -PathType Container) {
        foreach ($item in @(Get-ChildItem -LiteralPath $parent -Force -Directory)) {
            if ($item.Name -like ".$leaf-stage-*") {
                Remove-Item -LiteralPath $item.FullName -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
}

# Puts back the programs listed in $State: removes each new copy already placed and renames its
# older copy back. Returns $false when a name could not be given back its older copy; that name
# then keeps the new copy so it still holds a runnable program.
function Undo-WiPlacement {
    param($State)
    $restored = $true
    foreach ($name in $WiPrograms) {
        $dest = Join-Path $State.Dir $name
        $aside = Join-Path $State.Dir ".$name.old-$($State.Id)"
        $discard = Join-Path $State.Dir ".$name.bad-$($State.Id)"
        $isPlaced = $State.Placed -contains $name
        $isAside = $State.Aside -contains $name
        if (-not $isPlaced -and -not $isAside) { continue }
        try {
            if ($isPlaced) { [IO.File]::Move($dest, $discard) }
        } catch {
            $restored = $false
            continue
        }
        if ($isAside) {
            try {
                Invoke-WiTestHook "restore:$name"
                [IO.File]::Move($aside, $dest)
            } catch {
                $restored = $false
                if ($isPlaced) {
                    try { [IO.File]::Move($discard, $dest) } catch { Write-Verbose "The new $name could not be put back." }
                }
            }
        }
        Remove-Item -LiteralPath $discard -Force -ErrorAction SilentlyContinue
    }
    $State.Placed = @()
    $State.Aside = @()
    return $restored
}

# Replaces both installed programs with the staged ones, or neither. An existing program is renamed
# aside (a running program can be renamed), then the new one is moved in from the staging folder on
# the same volume. Any failure, including Ctrl+C, puts the older pair back.
function Install-WiPair {
    param([string]$Stage, [string]$Dir, [string]$Id)
    Assert-WiNotLinked $Dir
    $state = @{ Dir = $Dir; Id = $Id; Placed = @(); Aside = @() }
    $hadOlder = $false
    foreach ($name in $WiPrograms) {
        if (Test-Path -LiteralPath (Join-Path $Dir $name) -PathType Leaf) { $hadOlder = $true }
    }
    $committed = $false
    try {
        foreach ($name in $WiPrograms) {
            $dest = Join-Path $Dir $name
            if (Test-Path -LiteralPath $dest -PathType Leaf) {
                Invoke-WiTestHook "aside:$name"
                [IO.File]::Move($dest, (Join-Path $Dir ".$name.old-$Id"))
                $state.Aside += $name
            }
            Invoke-WiTestHook "place:$name"
            [IO.File]::Move((Join-Path $Stage $name), $dest)
            $state.Placed += $name
        }
        $committed = $true
    } catch {
        if (-not (Undo-WiPlacement $state)) { Invoke-WiFailure 'the installation is incomplete, so run this installer again.' }
        if ($hadOlder) {
            Invoke-WiFailure "the new programs could not be put in place, and the previous ones were kept; $WiKeep"
        }
        Invoke-WiFailure "the new programs could not be put in place, so nothing was installed; $WiKeep"
    } finally {
        if (-not $committed -and ($state.Placed.Count -gt 0 -or $state.Aside.Count -gt 0)) {
            if (-not (Undo-WiPlacement $state)) {
                Write-WiLine 'wiki-island could not be installed: the installation is incomplete, so run this installer again.'
            }
        }
    }
    foreach ($name in $WiPrograms) {
        Remove-Item -LiteralPath (Join-Path $Dir ".$name.old-$Id") -Force -ErrorAction SilentlyContinue
    }
}

# True when Path value $PathValue already holds folder $Dir (case-insensitive, ignoring a trailing
# backslash, with %VAR% references expanded for the comparison only).
function Test-WiPathEntry {
    param([string]$PathValue, [string]$Dir)
    $want = $Dir.TrimEnd('\')
    foreach ($entry in $PathValue.Split(';')) {
        $plain = $entry.Trim().TrimEnd('\')
        $expanded = [Environment]::ExpandEnvironmentVariables($entry.Trim()).TrimEnd('\')
        if ($plain -ieq $want -or $expanded -ieq $want) { return $true }
    }
    return $false
}

# Adds folder $Dir to the user Path once, keeping the stored text and value kind so %VAR%
# references are never expanded. Returns $true when it wrote the registry.
function Add-WiUserPath {
    param([string]$Dir)
    $failed = 'your user Path could not be updated, so new terminal windows would not find wi; run this installer again.'
    try {
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    } catch {
        Invoke-WiFailure $failed
    }
    try {
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        $current = ''
        if ($key.GetValueNames() -contains 'Path') {
            $kind = $key.GetValueKind('Path')
            $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        }
        if ($kind -ne [Microsoft.Win32.RegistryValueKind]::String -and $kind -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
            Invoke-WiFailure "your user Path has an unusual type and was left unchanged, so add $Dir to it in Windows settings, then run this installer again."
        }
        if (Test-WiPathEntry $current $Dir) { return $false }
        $updated = $Dir
        if ($current.Length -gt 0) {
            if ($current.EndsWith(';')) { $updated = "$current$Dir" } else { $updated = "$current;$Dir" }
        }
        try {
            $key.SetValue('Path', $updated, $kind)
        } catch {
            Invoke-WiFailure $failed
        }
    } finally {
        $key.Close()
    }
    # Setting a user variable through .NET broadcasts the settings change, so programs started
    # later from Explorer see the new Path. This throwaway variable exists only for that.
    try {
        [Environment]::SetEnvironmentVariable('WI_INSTALL_REFRESH', '1', 'User')
        [Environment]::SetEnvironmentVariable('WI_INSTALL_REFRESH', $null, 'User')
    } catch {
        Write-Verbose 'The settings change could not be announced; new sign-ins still see the Path.'
    }
    return $true
}

# Downloads and checks the release, then writes its programs into the new folder $Stage and runs
# each once with --version.
function Get-WiRelease {
    param([string]$Base, [string]$Temp, [string]$Stage)
    $tag = Get-WiLatestTag $Base
    switch ($tag) {
        'unreachable' { Invoke-WiFailure 'GitHub could not be reached, so nothing was installed; check your internet connection, then run this installer again.' }
        'none' { Invoke-WiFailure "no prebuilt wiki-island is published yet, so nothing was installed; to build it from source, follow $WiBuildDocs." }
        'badname' { Invoke-WiFailure "the latest release has an unexpected name, so nothing was installed. Run this installer again later, or report it at $WiIssues." }
    }

    $asset = "wiki-island-$tag-x86_64-pc-windows-msvc.zip"
    $zip = Join-Path $Temp $asset
    $manifest = Join-Path $Temp 'SHA256SUMS'
    Write-WiLine "Downloading wiki-island $tag..."
    switch (Get-WiDownload "$Base/releases/download/$tag/$asset" $zip (Get-WiAssetLimit)) {
        'toolarge' { Invoke-WiFailure "the downloaded program is larger than any release should be, so nothing was installed. Run this installer again later, or report it at $WiIssues." }
        'missing' { Invoke-WiFailure "this release has no program for Windows, so nothing was installed; to build it from source, follow $WiBuildDocs." }
        'error' { Invoke-WiFailure 'the download did not finish, so nothing was installed; check your internet connection, then run this installer again.' }
    }
    switch (Get-WiDownload "$Base/releases/download/$tag/SHA256SUMS" $manifest $WiMaxSumsBytes) {
        'toolarge' { Invoke-WiFailure "the published checksum list is larger than it should be, so nothing was installed. Run this installer again later, or report it at $WiIssues." }
        'missing' { Invoke-WiFailure "this release has no checksum list, so nothing was installed. Run this installer again later, or report it at $WiIssues." }
        'error' { Invoke-WiFailure 'the checksum list did not download, so nothing was installed; check your internet connection, then run this installer again.' }
    }

    $want = Get-WiExpectedDigest $manifest $asset
    $have = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($have -cne $want) {
        Invoke-WiFailure "the downloaded program does not match its checksum, so nothing was installed. Run this installer again later, or report it at $WiIssues."
    }

    Expand-WiPair $zip $Stage
    foreach ($name in $WiPrograms) {
        if ((Invoke-WiQuiet -Path (Join-Path $Stage $name) -Arguments '--version' -Seconds 60) -ne 0) {
            Invoke-WiFailure "the downloaded program does not run on this computer, so nothing was installed; report it at $WiIssues, or build it from source by following $WiBuildDocs."
        }
    }
}

# Runs the whole installation; every failure stops it with one plain sentence.
function Invoke-WiInstall {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        Invoke-WiFailure 'this installer is for Windows, so on macOS and Linux run scripts/install.sh instead.'
    }
    $arch = Get-WiArchitecture
    if ($arch -ne 'AMD64') {
        Invoke-WiFailure "there is no prebuilt wiki-island for this kind of Windows computer (only x86_64 is supported), so nothing was installed; to build it from source, follow $WiBuildDocs."
    }
    $dir = Get-WiInstallDir
    $base = [Environment]::GetEnvironmentVariable('WI_INSTALL_BASE_URL')
    if ([string]::IsNullOrEmpty($base)) { $base = $WiRepoUrl }
    $base = $base.TrimEnd('/')
    $baseUri = $null
    if (-not [Uri]::TryCreate($base, [UriKind]::Absolute, [ref]$baseUri) -or -not (Test-WiScheme $baseUri)) {
        Invoke-WiFailure 'the download address is not a web address, so nothing was installed; leave WI_INSTALL_BASE_URL unset, then run this installer again.'
    }

    # Windows PowerShell 5.1 may still default to TLS 1.0; GitHub needs TLS 1.2 or newer. The
    # operating system default (SystemDefault) already includes it and is left alone.
    $protocols = [Net.ServicePointManager]::SecurityProtocol
    if ($protocols -ne [Net.SecurityProtocolType]::SystemDefault) {
        [Net.ServicePointManager]::SecurityProtocol = $protocols -bor [Net.SecurityProtocolType]::Tls12
    }
    Assert-WiNotLinked $dir
    Clear-WiLeftover $dir

    $id = [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $temp = Join-Path ([IO.Path]::GetTempPath()) "wiki-island-install-$id"
    $parent = Split-Path -Parent $dir
    $stage = Join-Path $parent ".$(Split-Path -Leaf $dir)-stage-$id"
    $hadParent = Test-Path -LiteralPath $parent
    $isInstalled = $false
    try {
        try {
            [void][IO.Directory]::CreateDirectory($temp)
        } catch {
            Invoke-WiFailure "a working folder could not be created, so nothing was installed; $WiKeep"
        }
        Get-WiRelease -Base $base -Temp $temp -Stage $stage

        # The folder may have been swapped for a link while the release downloaded.
        Assert-WiNotLinked $dir
        if (Test-Path -LiteralPath $dir -PathType Leaf) {
            Invoke-WiFailure "a file named $(Split-Path -Leaf $dir) is in the way in $parent, so nothing was installed; move it aside, then run this installer again."
        }
        foreach ($name in $WiPrograms) {
            if (Test-Path -LiteralPath (Join-Path $dir $name) -PathType Container) {
                Invoke-WiFailure "a folder named $name is in the way in $dir, so nothing was installed; move it aside, then run this installer again."
            }
        }
        try {
            [void][IO.Directory]::CreateDirectory($dir)
        } catch {
            Invoke-WiFailure "the program folder could not be created, so nothing was installed; $WiKeep"
        }
        Install-WiPair -Stage $stage -Dir $dir -Id $id
        $isInstalled = $true
    } finally {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
        # A failed first install leaves no empty program folders behind.
        if (-not $isInstalled -and -not $hadParent) {
            Remove-Item -LiteralPath $parent -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    # A folder name with ';' or '%' cannot be stored safely in Path: ';' splits the entry and
    # '%name%' would be expanded as a variable. The Path is left alone then.
    $canUsePath = -not ($dir.Contains(';') -or $dir.Contains('%'))
    $added = $false
    if ($canUsePath) {
        $added = Add-WiUserPath $dir
        if (-not (Test-WiPathEntry $env:Path $dir)) { $env:Path = "$env:Path;$dir" }
    }

    # Connect your notes folder and AI apps (full path, so this works right now).
    if (-not (Test-WiFlag 'WI_INSTALL_SKIP_SETUP')) {
        # Native error output must not become a terminating error under Windows PowerShell 5.1.
        $ErrorActionPreference = 'Continue'
        & (Join-Path $dir 'wi.exe') setup
        $setupStatus = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($setupStatus -ne 0) {
            Invoke-WiFailure -Whole 'wiki-island was installed, but wi setup did not finish; follow its message above, then run wi setup again.'
        }
    }

    Write-WiLine ''
    Write-WiLine 'Done. Nothing else to do: talk to your AI as usual.'
    if (-not $canUsePath) {
        Write-WiLine "The programs are installed in $dir. That folder name cannot be added to your user Path, so run wi from there, or move the install by setting WI_INSTALL_DIR to a folder name without ; or % and running this installer again."
    } elseif ($added) {
        Write-WiLine "New terminal windows will find the wi command (its folder $dir was added to your user Path)."
    } else {
        Write-WiLine "New terminal windows will find the wi command (its folder $dir is on your user Path)."
    }
}

# Entry point. Reports a failure as one sentence and a terminating error, never by closing the
# PowerShell window it was pasted into.
function Install-WikiIsland {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    try {
        Invoke-WiInstall
    } catch {
        $failure = $_.Exception
        if ($failure.Data.Contains('WiPlain')) {
            Write-WiLine $failure.Message
        } else {
            Write-WiLine "wiki-island could not be installed: something unexpected went wrong, so run this installer again, or report it at $WiIssues (set WI_DEBUG=1 to see the details)."
            if (Test-WiFlag 'WI_DEBUG') { Write-WiLine ($_ | Out-String) }
        }
        Set-Variable -Name LASTEXITCODE -Value 1 -Scope Global
        throw 'The installer stopped; the message above says why.'
    }
}

Install-WikiIsland
