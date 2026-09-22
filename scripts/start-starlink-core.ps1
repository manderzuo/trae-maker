param(
  [string]$ReleaseRoot = '',
  [string]$DataDir = 'D:\gpt\starlink-dimension-router-data',
  [switch]$Background
)

$ErrorActionPreference = 'Stop'
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ([string]::IsNullOrWhiteSpace($ReleaseRoot)) {
  $ReleaseRoot = if (Test-Path -LiteralPath (Join-Path $scriptRoot 'release') -PathType Container) {
    $scriptRoot
  } else {
    'D:\gpt\starlink-core-persist-release'
  }
}
$exe = Join-Path $ReleaseRoot 'release\starlink-dimension-router.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
  throw "未找到 Core 程序: $exe"
}
if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
  New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
}

# 公网地址保存在 router.json；这里仅固定数据目录，避免依赖当前终端会话。
$env:STARLINK_ROUTER_DATA_DIR = $DataDir
Remove-Item Env:STARLINK_ROUTER_PUBLIC_BASE_URL -ErrorAction SilentlyContinue

if ($Background) {
  Start-Process -FilePath $exe -WorkingDirectory (Split-Path -Parent $exe) -WindowStyle Hidden
  exit 0
}

& $exe
exit $LASTEXITCODE
