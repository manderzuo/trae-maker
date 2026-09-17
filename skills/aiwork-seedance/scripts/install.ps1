[CmdletBinding()]
param(
    [string]$GatewayBaseUrl,
    [string]$ApiKey,
    [switch]$SkipConfig,
    [string]$InstallTo
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourceRoot = Split-Path -Parent $PSScriptRoot
$defaultInstall = Join-Path (Join-Path $env:USERPROFILE '.agents\skills') 'aiwork-seedance'
$target = if ($InstallTo) { [IO.Path]::GetFullPath($InstallTo) } else { $defaultInstall }
$configDir = Join-Path $env:APPDATA 'AIWork'
$configPath = Join-Path $configDir 'seedance-skill.json'

function Save-Config {
    param([string]$BaseUrl, [string]$PlainKey)
    if ([string]::IsNullOrWhiteSpace($BaseUrl)) { throw '网关地址不能为空。' }
    $BaseUrl = $BaseUrl.Trim().TrimEnd('/')
    if ($BaseUrl -notmatch '/v1$') { $BaseUrl = "$BaseUrl/v1" }
    if ([string]::IsNullOrWhiteSpace($PlainKey)) { throw 'API Key 不能为空。' }
    New-Item -ItemType Directory -Force -Path $configDir | Out-Null
    $secure = ConvertTo-SecureString -String $PlainKey -AsPlainText -Force
    $body = [ordered]@{
        gateway_base_url = $BaseUrl
        api_key_protected = (ConvertFrom-SecureString -SecureString $secure)
        updated_at = [DateTime]::UtcNow.ToString('o')
    }
    $body | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $configPath -Encoding UTF8
}

if (-not (Test-Path -LiteralPath $target)) {
    New-Item -ItemType Directory -Force -Path $target | Out-Null
}
if ([IO.Path]::GetFullPath($sourceRoot) -ne [IO.Path]::GetFullPath($target)) {
    Get-ChildItem -LiteralPath $sourceRoot -Force | Where-Object { $_.Name -notin @('.git', 'node_modules') } |
        Copy-Item -Destination $target -Recurse -Force
}

# The open Agent Skills path is canonical. Mirror only into known local paths;
# never overwrite an unknown client's configuration format.
$knownSkillRoots = @(
    (Join-Path (Join-Path $env:USERPROFILE '.codex\skills') 'aiwork-seedance'),
    (Join-Path (Join-Path $env:USERPROFILE '.claude\skills') 'aiwork-seedance')
)
foreach ($known in $knownSkillRoots) {
    if (Test-Path -LiteralPath (Split-Path -Parent $known)) {
        if ([IO.Path]::GetFullPath($sourceRoot) -ne [IO.Path]::GetFullPath($known)) {
            New-Item -ItemType Directory -Force -Path $known | Out-Null
            Get-ChildItem -LiteralPath $sourceRoot -Force | Where-Object { $_.Name -notin @('.git', 'node_modules') } |
                Copy-Item -Destination $known -Recurse -Force
        }
    }
}

if (-not $SkipConfig) {
    if (-not $GatewayBaseUrl) { $GatewayBaseUrl = $env:AIWORK_GATEWAY_BASE_URL }
    if (-not $ApiKey) { $ApiKey = $env:AIWORK_API_KEY }
    if (-not $GatewayBaseUrl) { $GatewayBaseUrl = Read-Host 'AI Work 网关地址（例如 https://example.com/v1）' }
    if (-not $ApiKey) { $ApiKey = Read-Host 'AI Work API Key（输入不会回显）' }
    Save-Config -BaseUrl $GatewayBaseUrl -PlainKey $ApiKey
}

Write-Output "AI Work Seedance Skill 已安装：$target"
Write-Output "配置文件：$configPath（API Key 使用 Windows DPAPI 加密）"
Write-Output '下一步可运行：powershell -File scripts\aiwork-seedance.ps1 doctor'
