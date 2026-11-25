#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$RepoUrl,
    [string]$Distro,
    [string]$Project,
    [switch]$Shortcut,
    [switch]$WhatIf
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 でも空白、引用符、末尾のバックスラッシュを argv に渡す。
function ConvertTo-NativeArgument([string]$Value) {
    if ($Value -and $Value -notmatch '[\s"]') { return $Value }
    return '"' + (($Value -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
}

function Invoke-SetupProcess {
    param([string]$Executable, [string[]]$Arguments, [switch]$Capture, [switch]$TestExitCode)
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.UseShellExecute = $false
    $start.Arguments = ($Arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' '
    $start.EnvironmentVariables['WSL_UTF8'] = '1'
    if ($Capture) {
        $start.RedirectStandardOutput = $true
        $start.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    }
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        if ($Capture) { $output = $process.StandardOutput.ReadToEnd() }
        $process.WaitForExit()
        if ($TestExitCode) { return ($process.ExitCode -eq 0) }
        if ($process.ExitCode -ne 0) {
            throw "$Executable が終了コード $($process.ExitCode) を返しました。"
        }
        if ($Capture) { return $output.Replace([string][char]0, '').TrimEnd([char[]]"`r`n") }
    } finally {
        $process.Dispose()
    }
}

function Invoke-Wsl {
    param([string[]]$Arguments, [switch]$Capture, [switch]$TestExitCode)
    Invoke-SetupProcess -Executable $wslExe -Arguments $Arguments -Capture:$Capture -TestExitCode:$TestExitCode
}

$source = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $source 'crates\fuzukue\Cargo.toml'
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw 'fuzukue の Windows 側チェックアウトから実行してください。'
}
if (-not $RepoUrl) {
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($git) {
        $RepoUrl = & $git.Source config --get remote.origin.url 2>$null | Select-Object -First 1
        if (-not $RepoUrl) {
            $RepoUrl = & $git.Source -C $source config --get remote.origin.url 2>$null | Select-Object -First 1
        }
    }
    if (-not $RepoUrl) { throw 'origin の URL が見つかりません。-RepoUrl に指定してください。' }
}
if ($Project -and (-not $Project.StartsWith('/') -or $Project -match '[\x00-\x1F\x7F]')) {
    throw '-Project には WSL 内の絶対パスを指定してください。'
}

$target = Join-Path $env:LOCALAPPDATA 'fuzukue\target'
$client = Join-Path $target 'release\fuzukue.exe'
$portableSettings = Join-Path $target 'release\settings.toml'
$oldPortableSettings = Join-Path $env:LOCALAPPDATA 'labo-gui\target\release\settings.toml'
$settingsLabel = if (Test-Path -LiteralPath $portableSettings -PathType Leaf) { $portableSettings } elseif (Test-Path -LiteralPath $oldPortableSettings -PathType Leaf) { "$oldPortableSettings → $portableSettings" } else { '%APPDATA%\fuzukue\settings.toml' }
$packages = 'build-essential pkg-config git curl ca-certificates procps latexmk texlive-lang-japanese texlive-latex-extra poppler-utils'
$distroLabel = if ($Distro) { $Distro } else { '既定のディストリビューション' }
$projectLabel = if ($Project) { $Project } else { '設定済みのパス、または ~/fuzukue/examples/lab' }
Write-Host "WSL ($distroLabel) の ~/fuzukue を更新してサーバーをビルド: $RepoUrl"
Write-Host "Windows クライアントのビルド先: $target"
Write-Host "設定: $settingsLabel (mode = wsl、project = $projectLabel)"
Write-Host "Ubuntu / Debian で不足があれば、WSL ターミナルで実行:"
Write-Host "  sudo apt update"
Write-Host "  sudo apt install $packages"
if ($Shortcut) { Write-Host 'スタートメニューに fuzukue のショートカットを作成' }
if ($WhatIf) { return }

$wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue
if (-not $wsl) { throw 'wsl.exe が見つかりません。Windows で wsl --install を実行してください。' }
$wslExe = $wsl.Source
$registered = (Invoke-Wsl -Arguments @('--list', '--quiet') -Capture) -split '\r?\n' |
    ForEach-Object { $_.Trim().TrimStart([char]0xFEFF) } | Where-Object { $_ }
if (-not $registered) { throw 'WSL ディストリビューションがありません。例: wsl --install -d Ubuntu' }
if ($Distro -match '^[\-]|[\s"]') { throw '-Distro に空白や引用符を含まない名前を指定してください。' }
if ($Distro -and $Distro -notin $registered) { throw "WSL ディストリビューションが見つかりません: $Distro" }
$distroArguments = @()
if ($Distro) { $distroArguments += @('-d', $Distro) }
$prefix = $distroArguments + @('--cd', '/', '--exec')
$actualDistro = Invoke-Wsl -Arguments ($prefix + @('/usr/bin/printenv', 'WSL_DISTRO_NAME')) -Capture
$linuxHome = Invoke-Wsl -Arguments ($prefix + @('/usr/bin/printenv', 'HOME')) -Capture
if (-not $actualDistro -or -not $linuxHome.StartsWith('/')) {
    throw 'WSL のディストリビューション名またはホームディレクトリを取得できません。'
}
# 注: 既定のクローン先を ~/fuzukue に変更。旧 ~/lab-gui は触らず残る（新規 clone になり、旧環境は壊れない。不要なら手で削除）。RepoUrl は origin から取るので、GitHub 側の改名後は新 URL になる。
$repo = $linuxHome.TrimEnd('/') + '/fuzukue'
$defaultProject = "$repo/examples/lab"

$windowsCargo = Get-Command cargo.exe -ErrorAction SilentlyContinue
if (-not $windowsCargo) { throw 'Windows に Rust（MSVC ツールチェーン）をインストールしてください。' }
if ((Read-Host '必要な apt パッケージを用意できたら、セットアップを続けますか [y/N]') -notmatch '^(y|yes)$') {
    return
}

$hasCheckout = Invoke-Wsl -Arguments ($prefix + @('/usr/bin/test', '-e', "$repo/.git")) -TestExitCode
if ($hasCheckout) {
    $origin = Invoke-Wsl -Arguments ($prefix + @('/usr/bin/git', '-C', $repo, 'remote', 'get-url', 'origin')) -Capture
    if ($origin -cne $RepoUrl) { throw "WSL 側の origin が指定 URL と異なります: $origin" }
    Invoke-Wsl -Arguments ($prefix + @('/usr/bin/git', '-C', $repo, 'pull', '--ff-only'))
} else {
    Invoke-Wsl -Arguments ($prefix + @('/usr/bin/git', 'clone', '--', $RepoUrl, $repo))
}
if ($Project -and -not (Invoke-Wsl -Arguments ($prefix + @('/usr/bin/test', '-d', $Project)) -TestExitCode)) {
    throw "プロジェクトが WSL 内に見つかりません: $Project"
}

$linuxCargo = "$linuxHome/.cargo/bin/cargo"
if (-not (Invoke-Wsl -Arguments ($prefix + @('/usr/bin/test', '-x', $linuxCargo)) -TestExitCode)) {
    if (Invoke-Wsl -Arguments ($prefix + @('/usr/bin/env', 'cargo', '--version')) -TestExitCode) {
        $linuxCargo = 'cargo'
    } else {
        if ((Read-Host "WSL ($actualDistro) に rustup をインストールしますか [y/N]") -notmatch '^(y|yes)$') {
            throw 'WSL に Rust を入れてから再実行してください。'
        }
        $installer = Invoke-Wsl -Arguments ($prefix + @('/usr/bin/mktemp')) -Capture
        try {
            Invoke-Wsl -Arguments ($prefix + @('/usr/bin/curl', '--proto', '=https', '--tlsv1.2', '-sSf',
                'https://sh.rustup.rs', '-o', $installer))
            Invoke-Wsl -Arguments ($prefix + @('/bin/sh', $installer, '-y', '--profile', 'minimal'))
        } finally {
            Invoke-Wsl -Arguments ($prefix + @('/usr/bin/rm', '-f', '--', $installer))
        }
    }
}

# 各 OS でネイティブのファイルシステムにビルド成果物を置く。
Invoke-Wsl -Arguments ($distroArguments + @('--cd', $repo, '--exec', '/usr/bin/env', $linuxCargo, 'build', '--release',
    '--manifest-path', "$repo/Cargo.toml", '--target-dir', "$repo/target", '-p', 'secretaire'))
Push-Location -LiteralPath $source
try {
    & $windowsCargo.Source build --release --manifest-path $manifest --target-dir $target
    if ($LASTEXITCODE -ne 0) { throw "Windows クライアントのビルドに失敗しました ($LASTEXITCODE)。" }
} finally {
    Pop-Location
}

if ((Test-Path -LiteralPath $oldPortableSettings -PathType Leaf) -and -not (Test-Path -LiteralPath $portableSettings)) {
    [IO.File]::Copy($oldPortableSettings, $portableSettings, $false)
    Write-Host "設定をコピー: $oldPortableSettings → $portableSettings"
}

# クライアント自身の設定処理で既存の外観設定を保ち、同じ形式に保存する。
$configArgs = @('--configure-wsl', $actualDistro, $repo, $defaultProject)
if ($Project) { $configArgs += $Project }
Invoke-SetupProcess -Executable $client -Arguments $configArgs

if ($Shortcut) {
    $programs = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $programs 'fuzukue.lnk'))
    $link.TargetPath = $client
    $link.WorkingDirectory = Split-Path -Parent $client
    $link.Save()
}
Write-Host "起動するには: $client"
