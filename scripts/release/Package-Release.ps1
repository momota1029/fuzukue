#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('\Av[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\z')]
    [string]$Tag,
    [Parameter(Mandatory)]
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
$output = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
[IO.Directory]::CreateDirectory($output) | Out-Null
$stage = Join-Path $output ('.windows-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $stage 'payload'
[IO.Directory]::CreateDirectory($payload) | Out-Null
$oldBuildId = $env:FUZUKUE_CLIENT_BUILD_ID
try {
    $commit = & git -C $repo rev-parse --verify 'HEAD^{commit}'
    if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve the source commit.' }
    $env:FUZUKUE_CLIENT_BUILD_ID = $commit
    $targetDir = Join-Path $stage 'target'
    $target = 'x86_64-pc-windows-msvc'
    $manifest = Join-Path $repo 'crates/fuzukue/Cargo.toml'
    & cargo build --locked --release --manifest-path $manifest --target $target --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw 'The Windows client build failed.' }
    [IO.File]::Copy((Join-Path $targetDir "$target/release/fuzukue.exe"), (Join-Path $payload 'fuzukue.exe'))
    $notices = @(
        'LICENSE-APACHE', 'LICENSE-MIT', 'NOTICE', 'THIRD_PARTY_NOTICES',
        'vendor/gpui-base/LICENSE-APACHE', 'vendor/gpui-pre/LICENSE-APACHE',
        'vendor/gpui-pre-windows/LICENSE-APACHE', 'vendor/gpui-pdf/LICENSE',
        'crates/fuzukue/assets/icons/LICENSE', 'crates/fuzukue/assets/icons/LICENSE-FEATHER'
    )
    foreach ($file in $notices) {
        $dest = Join-Path $payload $file
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($dest)) | Out-Null
        [IO.File]::Copy((Join-Path $repo $file), $dest)
    }
    $name = "fuzukue-$Tag-windows-x86_64.zip"
    $zip = Join-Path $stage $name
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($payload, $zip)
    Move-Item -LiteralPath $zip -Destination (Join-Path $output $name) -Force
} finally {
    $env:FUZUKUE_CLIENT_BUILD_ID = $oldBuildId
    Remove-Item -LiteralPath $stage -Recurse -Force
}
