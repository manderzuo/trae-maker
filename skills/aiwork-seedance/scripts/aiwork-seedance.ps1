[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('doctor', 'upload', 'submit', 'status', 'wait', 'download', 'generate')]
    [string]$Action = 'doctor',
    [string]$Prompt,
    [string]$TaskId,
    [string[]]$ImagePath,
    [string[]]$VideoPath,
    [string[]]$ImageAssetId,
    [string[]]$VideoAssetId,
    [string]$AssetPath,
    [string]$IdempotencyKey,
    [ValidateSet('480p', '720p', '1080p', '4k')]
    [string]$Resolution = '720p',
    [ValidateSet('16:9', '9:16', '1:1', '4:3', '3:4', '21:9')]
    [string]$Ratio = '16:9',
    [ValidateRange(2, 15)]
    [int]$Duration = 5,
    [ValidateRange(1, 86400)]
    [int]$TimeoutSeconds = 900,
    [ValidateRange(1, 30)]
    [int]$IntervalSeconds = 3,
    [string]$OutputPath,
    [string]$GatewayBaseUrl,
    [string]$ApiKey
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:ConfigPath = Join-Path (Join-Path $env:APPDATA 'AIWork') 'seedance-skill.json'
$script:Config = $null

function Read-Config {
    if ($script:Config) { return $script:Config }
    if (-not (Test-Path -LiteralPath $script:ConfigPath -PathType Leaf)) {
        $script:Config = [pscustomobject]@{}
        return $script:Config
    }
    try {
        $script:Config = Get-Content -LiteralPath $script:ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        throw "AI Work Skill 配置文件无法读取：$script:ConfigPath"
    }
    return $script:Config
}

function Get-SecretFromConfig {
    $config = Read-Config
    $protected = [string](Get-PropertyValue $config 'api_key_protected')
    if ([string]::IsNullOrWhiteSpace($protected)) { return '' }
    try {
        $secure = ConvertTo-SecureString -String $protected
        $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
        try {
            return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr)
        } finally {
            if ($ptr -ne [IntPtr]::Zero) {
                [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr)
            }
        }
    } catch {
        throw 'AI Work API Key 无法解密；请重新运行 install.cmd 配置。'
    }
}

function Get-ConfiguredBaseUrl {
    $configBase = Get-PropertyValue (Read-Config) 'gateway_base_url'
    $value = if ($GatewayBaseUrl) { $GatewayBaseUrl } elseif ($env:AIWORK_GATEWAY_BASE_URL) { $env:AIWORK_GATEWAY_BASE_URL } else { [string]$configBase }
    $value = ([string]$value).Trim().TrimEnd('/')
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw '未配置 AIWORK_GATEWAY_BASE_URL；请运行 install.cmd。'
    }
    if ($value -notmatch '/v1$') { $value = "$value/v1" }
    return $value.TrimEnd('/')
}

function Get-ConfiguredApiKey {
    $value = if ($ApiKey) { $ApiKey } elseif ($env:AIWORK_API_KEY) { $env:AIWORK_API_KEY } else { Get-SecretFromConfig }
    $value = ([string]$value).Trim()
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw '未配置 AIWORK_API_KEY；请运行 install.cmd。'
    }
    return $value
}

function Get-CommonHeaders {
    return @{
        Accept = 'application/json; charset=utf-8'
        Authorization = "Bearer $(Get-ConfiguredApiKey)"
    }
}

function Convert-ResponseBody($response) {
    $raw = [Text.Encoding]::UTF8.GetString($response.RawContentStream.ToArray())
    if ([string]::IsNullOrWhiteSpace($raw)) { return [pscustomobject]@{} }
    try { return $raw | ConvertFrom-Json } catch { return [pscustomobject]@{ message = $raw.Substring(0, [Math]::Min(500, $raw.Length)) } }
}

function Get-ErrorMessage($value) {
    if ($null -eq $value) { return '未知错误' }
    $errorValue = Get-PropertyValue $value 'error'
    if ($errorValue -and (Get-PropertyValue $errorValue 'message')) { return [string](Get-PropertyValue $errorValue 'message') }
    if (Get-PropertyValue $value 'message') { return [string](Get-PropertyValue $value 'message') }
    return (($value | ConvertTo-Json -Compress -Depth 8).Substring(0, [Math]::Min(500, (($value | ConvertTo-Json -Compress -Depth 8).Length))))
}

function Get-PropertyValue($value, [string]$name) {
    if ($null -eq $value) { return $null }
    $property = $value.PSObject.Properties[$name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Invoke-AiworkJson {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('GET', 'POST', 'DELETE')][string]$Method,
        [Parameter(Mandatory = $true)][string]$Uri,
        [AllowNull()][object]$Body,
        [hashtable]$ExtraHeaders
    )
    $headers = Get-CommonHeaders
    if ($ExtraHeaders) { foreach ($key in $ExtraHeaders.Keys) { $headers[$key] = [string]$ExtraHeaders[$key] } }
    $request = @{
        Method = $Method
        Uri = $Uri
        Headers = $headers
        UseBasicParsing = $true
        TimeoutSec = 60
    }
    $uriObject = [Uri]$Uri
    if ($uriObject.Host -in @('127.0.0.1', 'localhost', '::1')) { $request.Proxy = $null }
    if ($null -ne $Body) {
        $request.ContentType = 'application/json; charset=utf-8'
        $request.Body = $Body | ConvertTo-Json -Compress -Depth 30
    }
    try {
        $response = Invoke-WebRequest @request
        if ($response.StatusCode -lt 200 -or $response.StatusCode -ge 300) {
            throw "HTTP $($response.StatusCode)：$(Get-ErrorMessage (Convert-ResponseBody $response))"
        }
        return Convert-ResponseBody $response
    } catch {
        $detail = $_.Exception.Message
        if ($detail -match 'AI Work|HTTP') { throw $detail }
        throw "AI Work 网关请求失败：$detail"
    }
}

function Get-HealthUri {
    $base = Get-ConfiguredBaseUrl
    return ($base -replace '/v1$', '') + '/health'
}

function Get-ApiUri([string]$Path) {
    return "$(Get-ConfiguredBaseUrl)$Path"
}

function Write-Result($value) {
    $value | ConvertTo-Json -Depth 30
}

function Get-MimeType([string]$Path) {
    switch ([IO.Path]::GetExtension($Path).ToLowerInvariant()) {
        '.png' { return 'image/png' }
        '.jpg' { return 'image/jpeg' }
        '.jpeg' { return 'image/jpeg' }
        '.webp' { return 'image/webp' }
        '.gif' { return 'image/gif' }
        '.mp4' { return 'video/mp4' }
        '.webm' { return 'video/webm' }
        default { return 'application/octet-stream' }
    }
}

function Upload-Asset([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw '素材路径不能为空。' }
    $resolved = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path)
    $file = Get-Item -LiteralPath $resolved -ErrorAction Stop
    if (-not $file.PSIsContainer -and $file.Length -gt 32MB) { throw "素材超过 32 MiB 限制：$resolved" }
    if ($file.Length -eq 0) { throw "素材为空：$resolved" }
    $payload = @{
        filename = $file.Name
        mime_type = Get-MimeType $resolved
        data_base64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($resolved))
    }
    $result = Invoke-AiworkJson -Method POST -Uri (Get-ApiUri '/assets') -Body $payload
    $assetId = Get-PropertyValue $result 'id'
    if (-not $assetId) { throw "网关未返回素材 ID：$(Get-ErrorMessage $result)" }
    return [string]$assetId
}

function Get-TaskId($value) {
    $task = Get-PropertyValue $value 'task'
    $data = Get-PropertyValue $value 'data'
    $dataTask = Get-PropertyValue $data 'task'
    $candidates = @(
        (Get-PropertyValue $task 'id'),
        (Get-PropertyValue $task 'task_id'),
        (Get-PropertyValue $dataTask 'id'),
        (Get-PropertyValue $data 'id'),
        (Get-PropertyValue $value 'id'),
        (Get-PropertyValue $value 'task_id')
    )
    foreach ($candidate in $candidates) {
        if ($null -ne $candidate -and -not [string]::IsNullOrWhiteSpace([string]$candidate)) { return [string]$candidate }
    }
    return ''
}

function Get-Task([string]$Id) {
    if ([string]::IsNullOrWhiteSpace($Id)) { throw 'TaskId 不能为空。' }
    $result = Invoke-AiworkJson -Method GET -Uri (Get-ApiUri "/videos/$Id")
    $task = Get-PropertyValue $result 'task'
    $data = Get-PropertyValue $result 'data'
    $dataTask = Get-PropertyValue $data 'task'
    if ($task) { return $task }
    if ($dataTask) { return $dataTask }
    if ($data) { return $data }
    return $result
}

function Get-TaskContentUrl($task) {
    foreach ($name in @('content_url', 'video_url', 'resource_uri')) {
        $value = Get-PropertyValue $task $name
        if ($value -and -not [string]::IsNullOrWhiteSpace([string]$value)) { return [string]$value }
    }
    return ''
}

function Resolve-ContentUrl([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) { return '' }
    if ($Value -match '^https?://') { return $Value }
    $origin = (Get-ConfiguredBaseUrl) -replace '/v1$', ''
    return "$origin$Value"
}

function Submit-Task {
    if ([string]::IsNullOrWhiteSpace($Prompt)) { throw 'Prompt 不能为空。' }
    $payload = @{
        model = 'seedance'
        prompt = $Prompt.Trim()
        duration = $Duration
        resolution = $Resolution
        ratio = $Ratio
    }
    $imageIds = @()
    if ($ImageAssetId) { $imageIds += $ImageAssetId }
    if ($ImagePath) { foreach ($path in $ImagePath) { $imageIds += Upload-Asset $path } }
    if ($imageIds.Count -gt 0) { $payload.image_asset_ids = @($imageIds) }
    $videoIds = @()
    if ($VideoAssetId) { $videoIds += $VideoAssetId }
    if ($VideoPath) { foreach ($path in $VideoPath) { $videoIds += Upload-Asset $path } }
    if ($videoIds.Count -gt 0) { $payload.video_asset_ids = @($videoIds) }
    $key = if ($IdempotencyKey) { $IdempotencyKey } else { "seedance-$([Guid]::NewGuid().ToString())" }
    $result = Invoke-AiworkJson -Method POST -Uri (Get-ApiUri '/videos/generations') -Body $payload -ExtraHeaders @{ 'Idempotency-Key' = $key }
    $id = Get-TaskId $result
    if ([string]::IsNullOrWhiteSpace($id)) { throw "网关未返回任务 ID：$(Get-ErrorMessage $result)" }
    return [pscustomobject]@{ task_id = $id; status = 'submitted'; idempotency_key = $key }
}

function Wait-Task([string]$Id) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        $task = Get-Task $Id
        $state = ([string](Get-PropertyValue $task 'status')).ToLowerInvariant()
        if ($state -in @('completed', 'failed', 'cancelled', 'canceled')) {
            if ($state -ne 'completed') { throw "Seedance 任务失败：$(Get-ErrorMessage $task)" }
            return $task
        }
        $remaining = [Math]::Max(1, [int][Math]::Ceiling(($deadline - [DateTime]::UtcNow).TotalSeconds))
        Start-Sleep -Seconds ([Math]::Min($IntervalSeconds, $remaining))
    }
    throw "任务超过 $TimeoutSeconds 秒仍未完成：$Id"
}

function Download-Task([string]$Id, [string]$Target) {
    if ([string]::IsNullOrWhiteSpace($Target)) { throw 'OutputPath 不能为空。' }
    $task = Get-Task $Id
    $state = ([string](Get-PropertyValue $task 'status')).ToLowerInvariant()
    if ($state -ne 'completed') { throw "任务尚未完成，当前状态：$state" }
    $content = Resolve-ContentUrl (Get-TaskContentUrl $task)
    if ([string]::IsNullOrWhiteSpace($content)) { throw '任务已完成但没有可下载的视频地址。' }
    $destination = [IO.Path]::GetFullPath($Target)
    $parent = Split-Path -Parent $destination
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    $temp = "$destination.part"
    try {
        $downloadRequest = @{ Uri = $content; Headers = (Get-CommonHeaders); UseBasicParsing = $true; TimeoutSec = 120; OutFile = $temp }
        $downloadUri = [Uri]$content
        if ($downloadUri.Host -in @('127.0.0.1', 'localhost', '::1')) { $downloadRequest.Proxy = $null }
        Invoke-WebRequest @downloadRequest
        Move-Item -LiteralPath $temp -Destination $destination -Force
    } catch {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        throw "视频下载失败：$($_.Exception.Message)"
    }
    return [pscustomobject]@{ task_id = $Id; status = 'completed'; content_url = $content; download_path = $destination }
}

try {
    switch ($Action) {
        'doctor' {
            $health = Invoke-AiworkJson -Method GET -Uri (Get-HealthUri)
            Write-Result ([pscustomobject]@{ ok = $true; gateway = (Get-ConfiguredBaseUrl); health = $health })
        }
        'upload' {
            if (-not $AssetPath) { throw 'upload 需要 -AssetPath。' }
            Write-Result ([pscustomobject]@{ asset_id = (Upload-Asset $AssetPath) })
        }
        'submit' {
            Write-Result (Submit-Task)
        }
        'status' {
            Write-Result (Get-Task $TaskId)
        }
        'wait' {
            Write-Result (Wait-Task $TaskId)
        }
        'download' {
            Write-Result (Download-Task $TaskId $OutputPath)
        }
        'generate' {
            $submitted = Submit-Task
            $task = Wait-Task $submitted.task_id
            if ($OutputPath) { Write-Result (Download-Task $submitted.task_id $OutputPath) }
            else { Write-Result ([pscustomobject]@{ task_id = $submitted.task_id; status = 'completed'; content_url = (Resolve-ContentUrl (Get-TaskContentUrl $task)) }) }
        }
    }
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
