param(
    [Parameter(Mandatory=$true)][string]$ArchivePath,
    [Parameter(Mandatory=$true)][string]$ToolsDirectory
)
$ErrorActionPreference = 'Stop'
$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot '../build-assets/ffmpeg-release.json') -Raw | ConvertFrom-Json
$archive = (Resolve-Path -LiteralPath $ArchivePath).Path
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ine $manifest.archive_sha256) {
    throw 'FFmpeg archive fingerprint mismatch; nothing installed'
}
if (![IO.Path]::IsPathRooted($ToolsDirectory)) { throw 'ToolsDirectory must be an absolute, dedicated directory' }
$toolsRoot = [IO.Path]::GetFullPath($ToolsDirectory)
[IO.Directory]::CreateDirectory($toolsRoot) | Out-Null
$destination = Join-Path $toolsRoot $manifest.version
$executable = Join-Path $destination 'ffmpeg.exe'
# Never overwrite an installed version, discover programs on PATH, copy from
# another application, update production configuration, or restart services.
if (Test-Path -LiteralPath $destination) {
    if (!(Test-Path -LiteralPath $executable) -or
        (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash -ine $manifest.executable_sha256) {
        throw 'Installed version differs from the release lock; preserve it for investigation'
    }
} else {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    $stage = Join-Path $toolsRoot ($manifest.version + '-staging-' + [guid]::NewGuid().ToString('N'))
    try {
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        foreach ($item in @{
            'bin/ffmpeg.exe'='ffmpeg.exe';
            'LICENSE'='LICENSE'; 'README.txt'='README.txt'
        }.GetEnumerator()) {
            $entry = $zip.GetEntry($manifest.archive_root + '/' + $item.Key)
            if (!$entry -or $entry.Length -le 0 -or $entry.Length -gt 268435456) { throw 'Pinned archive layout is invalid' }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $stage $item.Value), $false)
        }
        if ((Get-FileHash -LiteralPath (Join-Path $stage 'ffmpeg.exe') -Algorithm SHA256).Hash -ine $manifest.executable_sha256) {
            throw 'FFmpeg executable fingerprint mismatch; staging files retained, not activated'
        }
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../build-assets/ffmpeg-release.json') -Destination (Join-Path $stage 'release-lock.json')
        [IO.Directory]::Move($stage, $destination)
    } finally { $zip.Dispose() }
}
# A pending configuration is an artifact, not a write to the live data root.
$pending = Join-Path $destination 'video-frame-extractor.pending.json'
$configuration = @{path=$executable;sha256=$manifest.executable_sha256} | ConvertTo-Json -Compress
if (Test-Path -LiteralPath $pending) {
    $existing = Get-Content -LiteralPath $pending -Raw | ConvertFrom-Json
    if ($existing.path -cne $executable -or $existing.sha256 -ine $manifest.executable_sha256) {throw 'Existing pending configuration differs; preserved without overwrite'}
} else { [IO.File]::WriteAllText($pending,$configuration,[Text.UTF8Encoding]::new($false)) }
Write-Output "Independent FFmpeg installed: $executable"
Write-Output "Pending configuration (not activated): $pending"
