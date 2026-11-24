#Requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateSet('beta', 'stable')]
    [string]$Channel = $(if ($env:FUZUKUE_CHANNEL) { $env:FUZUKUE_CHANNEL } else { 'beta' }),
    [string]$Version = $env:FUZUKUE_VERSION,
    [ValidatePattern('\A[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+\z')]
    [string]$Repository = 'momota1029/fuzukue',
    [uri]$Server,
    [string]$SettingsFrom,
    [switch]$Shortcut,
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$number = '(?:0|[1-9][0-9]*)'
$identifier = "(?:$number|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
$tagPattern = "\Av($number)\.($number)\.($number)(?:-($identifier(?:\.$identifier)*))?\z"
$commitPattern = '\A(?:[0-9a-f]{40}|[0-9a-f]{64})\z'
$hashPattern = '\A[0-9a-f]{64}\z'
$serverVersionPattern = '\Aserver-(?:[0-9a-f]{40}|[0-9a-f]{64})-[0-9a-f]{64}\z'
$platform = 'windows-x86_64'
$manifestName = 'release-manifest.json'
$markerName = '.fuzukue-package'
$header = '# Managed by fuzukue release installer'
$powerShell = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'

function Assert-PlainPath([string]$Path, [switch]$Directory) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($item -and (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                    ($item.PSIsContainer -ne $Directory.IsPresent))) {
        throw "Unexpected path type: $Path"
    }
}

function Assert-Managed([string]$Path, [string]$FirstLine) {
    Assert-PlainPath $Path
    if ([IO.File]::Exists($Path) -and (Get-Content -LiteralPath $Path -TotalCount 1) -cne $FirstLine) {
        throw "Unmanaged launcher: $Path"
    }
}

function Get-Asset([string]$Url, [string]$Path, [switch]$FromServer) {
    $curlArgs = @('--disable', '--fail', '--silent', '--show-error', '--proto', '=https',
                  '--proto-redir', '=https', '--connect-timeout', '15', '--max-time', '600',
                  '--output', $Path)
    $redirects = if ($FromServer) { '0' } else { '10' }
    $curlArgs += @('--location', '--max-redirs', $redirects)
    $token = $env:FUZUKUE_INSTALL_TOKEN
    if ($FromServer -and $token) {
        if ($token -cnotmatch '\A[A-Za-z0-9._~+/-]+=*\z') { throw 'Invalid bearer token format.' }
        "header = `"Authorization: Bearer $token`"" | & curl.exe @curlArgs --config - $Url
    } else {
        & curl.exe @curlArgs $Url
    }
    if ($LASTEXITCODE -ne 0) { throw "Download failed: $Url (curl exit $LASTEXITCODE)" }
}

function Get-Json([string]$Url, [switch]$FromServer) {
    $path = Join-Path $stage 'response.json'
    Get-Asset $Url $path -FromServer:$FromServer
    return (Get-Content -LiteralPath $path -Raw | ConvertFrom-Json)
}

function Compare-Tags([string]$Left, [string]$Right) {
    $a = [regex]::Match($Left, $tagPattern).Groups
    $b = [regex]::Match($Right, $tagPattern).Groups
    foreach ($i in 1..3) {
        $order = ([bigint]::Parse($a[$i].Value)).CompareTo([bigint]::Parse($b[$i].Value))
        if ($order) { return $order }
    }
    if (!$a[4].Success -or !$b[4].Success) {
        return ([int](!$a[4].Success)).CompareTo([int](!$b[4].Success))
    }
    $partsA = $a[4].Value.Split('.')
    $partsB = $b[4].Value.Split('.')
    for ($i = 0; $i -lt [Math]::Min($partsA.Length, $partsB.Length); $i++) {
        $numericA = $partsA[$i] -cmatch '\A[0-9]+\z'
        $numericB = $partsB[$i] -cmatch '\A[0-9]+\z'
        if ($numericA -and $numericB) {
            $order = ([bigint]::Parse($partsA[$i])).CompareTo([bigint]::Parse($partsB[$i]))
        } elseif ($numericA -ne $numericB) {
            $order = ([int]$numericB).CompareTo([int]$numericA)
        } else {
            $order = [string]::CompareOrdinal($partsA[$i], $partsB[$i])
        }
        if ($order) { return $order }
    }
    return $partsA.Length.CompareTo($partsB.Length)
}

function Test-Release($Release) {
    $tag = [string]$Release.tag_name
    if ($Release.draft -or $tag -cnotmatch $tagPattern) { return $false }
    $beta = $tag.Contains('-')
    if ($Release.prerelease -ne $beta -or ($beta -and $Channel -eq 'stable')) { return $false }
    $names = @($Release.assets | ForEach-Object { $_.name })
    return ($names -ccontains "fuzukue-$tag-$platform.zip" -and $names -ccontains 'SHA256SUMS' -and
            $names -ccontains $manifestName)
}

function Find-Tag([string]$Current) {
    if ($Version) {
        $release = Get-Json "$api/releases/tags/$Version"
        if ($release.tag_name -cne $Version -or !(Test-Release $release)) {
            throw "No eligible release for $Version."
        }
        return $Version
    }
    $latest = ''
    $beta = ''
    for ($page = 1; $page -le 1000; $page++) {
        $rows = @(Get-Json "$api/releases?per_page=100&page=$page")
        foreach ($row in $rows) {
            if (!(Test-Release $row)) { continue }
            $tag = [string]$row.tag_name
            if ($Current -cmatch $tagPattern -and (Compare-Tags $tag $Current) -lt 0) { continue }
            if (!$latest -or (Compare-Tags $tag $latest) -gt 0) { $latest = $tag }
            if ($tag.Contains('-') -and (!$beta -or (Compare-Tags $tag $beta) -gt 0)) { $beta = $tag }
        }
        if ($rows.Count -lt 100) { break }
    }
    if ($page -gt 1000) { throw 'Too many release pages. Set FUZUKUE_VERSION.' }
    if ($Current -cnotmatch $tagPattern -and $Channel -eq 'beta' -and $beta) { return $beta }
    if (!$latest) { throw 'No eligible release. Use FUZUKUE_VERSION to select an older tag.' }
    return $latest
}

function Resolve-TagCommit([string]$Tag) {
    $ref = Get-Json "$api/git/ref/tags/$Tag"
    if ($ref.ref -cne "refs/tags/$Tag") { throw 'GitHub returned a different tag reference.' }
    $object = $ref.object
    for ($depth = 0; $depth -lt 10; $depth++) {
        if ($object.sha -cnotmatch $commitPattern) { throw 'Invalid Git object SHA.' }
        if ($object.type -ceq 'commit') { return $object.sha }
        if ($object.type -cne 'tag') { break }
        $object = (Get-Json "$api/git/tags/$($object.sha)").object
    }
    throw 'Release tag must resolve to a commit.'
}

function Check-Sum([string]$Path, [string[]]$Sums) {
    $name = [IO.Path]::GetFileName($Path)
    $pattern = '\A([0-9a-fA-F]{64}) [ *]' + [regex]::Escape($name) + '\z'
    $entries = @(
        foreach ($line in $Sums) {
            if ($line -cmatch $pattern) { $Matches[1].ToLowerInvariant() }
        }
    )
    if ($entries.Count -ne 1) { throw "Expected one SHA256SUMS entry for $name." }
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $entries[0]) { throw "SHA-256 mismatch for $name." }
    return $actual
}

function Copy-ExecutableFromZip([string]$Zip, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Zip)
    try {
        $entries = @($archive.Entries | Where-Object { $_.FullName -ceq 'fuzukue.exe' })
        if ($entries.Count -ne 1 -or $entries[0].Length -le 0 -or
            ($entries[0].ExternalAttributes -band 0x400) -or
            (($entries[0].ExternalAttributes -shr 16) -band 0xf000) -eq 0xa000) {
            throw 'Expected one regular fuzukue.exe in the ZIP.'
        }
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], $Destination)
    } finally {
        $archive.Dispose()
    }
}

function Set-Current([string]$Tag) {
    $next = Join-Path $stage 'next-current'
    [IO.File]::WriteAllText($next, "$Tag`n", [Text.Encoding]::ASCII)
    if ([IO.File]::Exists($pointer)) {
        [IO.File]::Replace($next, $pointer, (Join-Path $stage 'old-current'))
    } else {
        [IO.File]::Move($next, $pointer)
    }
}

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'Windows x86_64 PowerShell is required.' }
if ($Channel -notin @('beta', 'stable')) { throw 'FUZUKUE_CHANNEL must be beta or stable.' }
if ($Version -and $Version -cnotmatch $tagPattern) { throw 'FUZUKUE_VERSION must be a v-prefixed SemVer tag.' }
if (($Repository -split '/')[1] -in @('.', '..')) { throw 'Expected a GitHub OWNER/REPO.' }
if ($Server -and (!$Server.IsAbsoluteUri -or $Server.Scheme -ne 'https' -or $Server.UserInfo -or
                  $Server.Query -or $Server.Fragment -or $Version)) {
    throw 'Use an HTTPS server base URL without credentials, query, fragment, or FUZUKUE_VERSION.'
}

$base = Join-Path $env:LOCALAPPDATA 'fuzukue'
$install = Join-Path $base 'install'
$root = Join-Path $install 'client'
foreach ($dir in @($base, $install, $root)) { Assert-PlainPath $dir -Directory }
if ($Uninstall -and ![IO.Directory]::Exists($root)) { return }
[IO.Directory]::CreateDirectory($root) | Out-Null
$lockPath = Join-Path $root '.install.lock'
Assert-PlainPath $lockPath
$lock = $null
$deadline = [DateTime]::UtcNow.AddSeconds(30)
while (!$lock) {
    try {
        $lock = [IO.File]::Open($lockPath, 'OpenOrCreate', 'ReadWrite', 'None')
    } catch [IO.IOException] {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Timed out waiting for the install lock.' }
        Start-Sleep -Milliseconds 200
    }
}

$stage = Join-Path $root ('.stage-' + [Guid]::NewGuid().ToString('N'))
try {
    $api = "https://api.github.com/repos/$Repository"
    $pointer = Join-Path $root 'current'
    $launcher = Join-Path $root 'Launch-Fuzukue.ps1'
    $cmd = Join-Path $root 'fuzukue.cmd'
    $programs = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
    $linkPath = Join-Path $programs 'fuzukue Release.lnk'
    $linkArgs = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$launcher`""
    Assert-PlainPath $pointer
    Assert-Managed $launcher $header
    Assert-Managed $cmd '@rem Managed by fuzukue release installer'
    if ($Shortcut -or $Uninstall) {
        Assert-PlainPath $linkPath
        $shell = New-Object -ComObject WScript.Shell
        $link = $shell.CreateShortcut($linkPath)
        if ([IO.File]::Exists($linkPath) -and ($link.TargetPath -ine $powerShell -or $link.Arguments -cne $linkArgs)) {
            throw "Unmanaged shortcut: $linkPath"
        }
    }

    $current = ''
    if ([IO.File]::Exists($pointer)) {
        $current = [IO.File]::ReadAllText($pointer).TrimEnd([char[]]"`r`n")
        if ($current -cnotmatch $tagPattern -and $current -cnotmatch $serverVersionPattern) {
            throw 'Invalid current release pointer.'
        }
        $currentDir = Join-Path $root $current
        Assert-PlainPath $currentDir -Directory
        $currentMarker = Join-Path $currentDir $markerName
        Assert-PlainPath $currentMarker
        if (!$Uninstall -and ![IO.File]::Exists($currentMarker)) { throw 'The current release is unmanaged.' }
    }
    if ($Uninstall) {
        # Remove only files owned by this installer. Keep settings and updater state.
        foreach ($dir in Get-ChildItem -LiteralPath $root -Directory -Force) {
            if (($dir.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                ($dir.Name -cnotmatch $tagPattern -and $dir.Name -cnotmatch $serverVersionPattern)) { continue }
            $marker = Join-Path $dir.FullName $markerName
            Assert-PlainPath $marker
            if (![IO.File]::Exists($marker) -or [IO.File]::ReadAllText($marker).Trim() -cnotmatch $hashPattern) { continue }
            $exe = Join-Path $dir.FullName 'fuzukue.exe'
            Assert-PlainPath $exe
            [IO.File]::Delete($exe)
            if (@(Get-ChildItem -LiteralPath $dir.FullName -Force).Count -eq 1) {
                [IO.File]::Delete($marker)
                [IO.Directory]::Delete($dir.FullName)
            }
        }
        foreach ($file in @($pointer, $cmd, $launcher, $linkPath)) { [IO.File]::Delete($file) }
        Write-Host 'Uninstalled fuzukue. Settings and data were kept.'
        return
    }

    [IO.Directory]::CreateDirectory($stage) | Out-Null
    $payload = Join-Path $stage 'payload'
    [IO.Directory]::CreateDirectory($payload) | Out-Null
    $exe = Join-Path $payload 'fuzukue.exe'

    if ($Server) {
        $endpoint = $Server.AbsoluteUri.TrimEnd('/') + "/client/$platform"
        $manifest = Get-Json "$endpoint/manifest" -FromServer
        if ($manifest.platform -cne $platform -or $manifest.commit -cnotmatch $commitPattern -or
            $manifest.sha256 -cnotmatch $hashPattern -or $manifest.size -le 0) { throw 'Invalid server manifest.' }
        $checksum = $manifest.sha256
        Get-Asset "${endpoint}?sha256=$checksum" $exe -FromServer
        if ((Get-Item -LiteralPath $exe).Length -ne $manifest.size -or
            (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ine $checksum) {
            throw 'Server client size or SHA-256 mismatch.'
        }
        $tag = "server-$($manifest.commit)-$checksum"
    } else {
        $tag = Find-Tag $current
        $commit = Resolve-TagCommit $tag
        $url = "https://github.com/$Repository/releases/download/$tag"
        $sumsPath = Join-Path $stage 'SHA256SUMS'
        $manifestPath = Join-Path $stage $manifestName
        Get-Asset "$url/SHA256SUMS" $sumsPath
        Get-Asset "$url/$manifestName" $manifestPath
        $sums = Get-Content -LiteralPath $sumsPath
        Check-Sum $manifestPath $sums | Out-Null
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        $releaseChannel = if ($tag.Contains('-')) { 'beta' } else { 'stable' }
        if ($manifest.schema_version -ne 1 -or "v$($manifest.version)" -cne $tag -or
            $manifest.commit -cne $commit -or $manifest.channel -cne $releaseChannel -or
            $manifest.min_client_protocol -le 0 -or $manifest.min_server_protocol -le 0 -or
            $manifest.min_client_protocol -gt $manifest.protocol -or $manifest.min_server_protocol -gt $manifest.protocol) {
            throw 'Release manifest tag, commit, channel or protocol bounds do not match.'
        }
        $name = "fuzukue-$tag-$platform.zip"
        $assets = @($manifest.artifacts | Where-Object { $_.kind -ceq 'windows-client' -and $_.platform -ceq $platform })
        if ($assets.Count -ne 1 -or $assets[0].name -cne $name) { throw "Expected one manifest entry for $name." }
        $zip = Join-Path $stage $name
        Get-Asset "$url/$name" $zip
        $checksum = Check-Sum $zip $sums
        if ($checksum -cne $assets[0].sha256 -or (Get-Item -LiteralPath $zip).Length -ne $assets[0].size) {
            throw 'Release manifest size or SHA-256 mismatch.'
        }
        Copy-ExecutableFromZip $zip $exe
    }

    $dest = Join-Path $root $tag
    Assert-PlainPath $dest -Directory
    if ([IO.Directory]::Exists($dest)) {
        $marker = Join-Path $dest $markerName
        $installedExe = Join-Path $dest 'fuzukue.exe'
        Assert-PlainPath $marker
        Assert-PlainPath $installedExe
        if (![IO.File]::Exists($marker) -or [IO.File]::ReadAllText($marker).Trim() -cne $checksum) {
            throw "Existing version differs: $dest"
        }
        # A running server's client updater can replace this executable in place.
        if (![IO.File]::Exists($installedExe)) { [IO.File]::Move($exe, $installedExe) }
    } else {
        [IO.File]::WriteAllText((Join-Path $payload $markerName), "$checksum`n", [Text.Encoding]::ASCII)
        [IO.Directory]::Move($payload, $dest)
    }

    $settingsDir = Join-Path $env:APPDATA 'fuzukue'
    $settings = Join-Path $settingsDir 'settings.toml'
    $legacyDir = Join-Path $env:APPDATA 'labo-gui'
    if (![IO.File]::Exists($settings) -and ![IO.Directory]::Exists($legacyDir)) {
        $sources = @(
            (Join-Path $base 'target/release/settings.toml'),
            (Join-Path $env:USERPROFILE 'fuzukue-target/release/settings.toml')
        )
        if ($SettingsFrom) { $sources = @($SettingsFrom) }
        foreach ($source in $sources) {
            if (![IO.File]::Exists($source)) { continue }
            [IO.Directory]::CreateDirectory($settingsDir) | Out-Null
            [IO.File]::Copy($source, $settings, $false)
            break
        }
    }

    if (![IO.File]::Exists($launcher)) {
        $launcherText = @'
# Managed by fuzukue release installer
$ErrorActionPreference = 'Stop'
$version = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'current')).TrimEnd([char[]]"`r`n")
if ($version -cnotmatch '\A(?:v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?|server-[0-9a-f]{40,64}-[0-9a-f]{64})\z') {
    throw 'Invalid fuzukue release pointer.'
}
$dir = Get-Item -LiteralPath (Join-Path $PSScriptRoot $version) -Force
$exe = Get-Item -LiteralPath (Join-Path $dir.FullName 'fuzukue.exe') -Force
if (!$dir.PSIsContainer -or $exe.PSIsContainer -or
    (($dir.Attributes -bor $exe.Attributes) -band [IO.FileAttributes]::ReparsePoint)) {
    throw 'Invalid fuzukue release executable.'
}
& $exe.FullName @args
'@
        $nextLauncher = Join-Path $stage 'Launch-Fuzukue.ps1'
        [IO.File]::WriteAllText($nextLauncher, $launcherText, [Text.Encoding]::ASCII)
        [IO.File]::Move($nextLauncher, $launcher)
    }
    if (![IO.File]::Exists($cmd)) {
        # Preparing the command launcher is independent from the PowerShell launcher.
        $nextCmd = Join-Path $stage 'fuzukue.cmd'
        $cmdText = @'
@rem Managed by fuzukue release installer
@echo off
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0Launch-Fuzukue.ps1" %*
'@
        [IO.File]::WriteAllText($nextCmd, ($cmdText -replace '\r?\n', "`r`n"), [Text.Encoding]::ASCII)
        [IO.File]::Move($nextCmd, $cmd)
    }
    Set-Current $tag
    if ($Shortcut) {
        $link.TargetPath = $powerShell
        $link.Arguments = $linkArgs
        $link.Save()
    }
    Write-Host "Installed $tag. Launch with $cmd"
} finally {
    try {
        if ([IO.Directory]::Exists($stage)) { Remove-Item -LiteralPath $stage -Recurse -Force }
    } finally {
        $lock.Dispose()
    }
}
