# Shared by the CLI and MCP runner. Never send a generation to refresh an asset.
function Get-AiworkSha256([byte[]]$Bytes) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-AiworkReferenceDirectory {
    $base = Get-ConfiguredBaseUrl
    $key = Get-ConfiguredApiKey
    $scope = (Get-AiworkSha256 ([Text.Encoding]::UTF8.GetBytes("$base`n$key"))).Substring(0, 32)
    $directory = Join-Path (Join-Path $env:APPDATA 'AIWork/seedance-references') $scope
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    return $directory
}

function Write-AiworkReferenceJson([string]$Path, $Value) {
    $temp = "$Path.$([Guid]::NewGuid().ToString('N').Substring(0, 8)).tmp"
    try {
        [IO.File]::WriteAllText($temp, ($Value | ConvertTo-Json -Depth 20 -Compress), [Text.UTF8Encoding]::new($false))
        if ([IO.File]::Exists($Path)) { [IO.File]::Replace($temp, $Path, [NullString]::Value) }
        else { [IO.File]::Move($temp, $Path) }
    } finally { if ([IO.File]::Exists($temp)) { [IO.File]::Delete($temp) } }
}

function Read-AiworkReferenceJson([string]$Path) {
    if (-not [IO.File]::Exists($Path)) { return $null }
    if ((Get-Item -LiteralPath $Path).Length -gt 65536) { throw 'REFERENCE_RECORD_INVALID' }
    return ([IO.File]::ReadAllText($Path, [Text.Encoding]::UTF8) | ConvertFrom-Json)
}

function Get-AiworkReferenceFile([string]$Path, [string]$ExpectedSha256 = '') {
    $resolved = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path)
    $file = Get-Item -LiteralPath $resolved -ErrorAction Stop
    if ($file.PSIsContainer -or $file.Length -le 0 -or $file.Length -gt 32MB) { throw 'REFERENCE_FILE_INVALID' }
    $bytes = [IO.File]::ReadAllBytes($resolved)
    $hash = Get-AiworkSha256 $bytes
    if ($ExpectedSha256 -and $hash -cne $ExpectedSha256) { throw 'REFERENCE_CHANGED: 原始参考素材内容已变化，请重新选择。' }
    return [pscustomobject]@{ path = $resolved; sha256 = $hash; bytes = $bytes; mime = (Get-MimeType $resolved) }
}

function Save-AiworkReferenceSource([string]$AssetId, [string]$Path, [string]$Sha256) {
    if ($AssetId -notmatch '^[-A-Za-z0-9._~]{1,256}$') { throw 'REFERENCE_UPLOAD_INVALID' }
    $file = Get-AiworkReferenceFile $Path $Sha256
    $directory = Get-AiworkReferenceDirectory
    $name = Get-AiworkSha256 ([Text.Encoding]::UTF8.GetBytes($AssetId))
    $prior = Read-AiworkReferenceJson (Join-Path $directory "asset-$name.json")
    if ($prior -and ($prior.assetId -cne $AssetId -or $prior.sha256 -cne $file.sha256)) { throw 'REFERENCE_IDENTITY_CONFLICT' }
    Write-AiworkReferenceJson (Join-Path $directory "asset-$name.json") ([ordered]@{
        schemaVersion = 1; assetId = $AssetId; sourcePath = $file.path; sha256 = $file.sha256; bytes = $file.bytes.Length; mimeType = $file.mime
    })
}

function Resolve-AiworkReferenceSource([string]$AssetId, [string]$MediaType) {
    $directory = Get-AiworkReferenceDirectory
    $name = Get-AiworkSha256 ([Text.Encoding]::UTF8.GetBytes($AssetId))
    $record = Read-AiworkReferenceJson (Join-Path $directory "asset-$name.json")
    if (-not $record -or $record.schemaVersion -ne 1 -or $record.assetId -cne $AssetId) {
        throw 'REFERENCE_SOURCE_REQUIRED: 此临时素材 ID 没有当前网关账号的原文件记录，请传入明确的本地素材路径。'
    }
    $file = Get-AiworkReferenceFile $record.sourcePath $record.sha256
    if (-not $file.mime.StartsWith("$MediaType/")) { throw 'REFERENCE_MEDIA_MISMATCH' }
    return $file
}

function New-AiworkHttpError($Record) {
    if ($Record.Exception.Data.Contains('AiworkSafeError')) { return $Record.Exception }
    $response = Get-PropertyValue $Record.Exception 'Response'
    $status = 0
    if ($response) { try { $status = [int](Get-PropertyValue $response 'StatusCode') } catch {} }
    $raw = ''
    $details = Get-PropertyValue $Record 'ErrorDetails'
    if ($details) { $raw = [string](Get-PropertyValue $details 'Message') }
    if (-not $raw -and $response) {
        try {
            $stream = $response.GetResponseStream()
            if ($stream) {
                try {
                    $buffer = [byte[]]::new(65536)
                    $count = 0
                    while ($count -lt $buffer.Length) {
                        $read = $stream.Read($buffer, $count, $buffer.Length - $count)
                        if ($read -eq 0) { break }; $count += $read
                    }
                    $raw = [Text.Encoding]::UTF8.GetString($buffer, 0, $count)
                } finally { $stream.Dispose() }
            }
        } catch {}
    }
    $parsed = $null
    if ($raw.Length -le 65536) { try { $parsed = $raw | ConvertFrom-Json } catch {} }
    $errorValue = Get-PropertyValue $parsed 'error'
    $code = [string](Get-PropertyValue $errorValue 'code')
    $allowed = @('reference_asset_expired','reference_asset_missing','reference_asset_unavailable','reference_resource_unavailable',
        'reference_asset_type_mismatch','invalid_reference_context','quota_insufficient','budget_policy_expired','invalid_request',
        'idempotency_conflict','video_execution_failed','video_safety_check_failed','reference_safety_check_failed','prompt_safety_check_failed')
    if ($code -cnotin $allowed) { $code = if ($status -gt 0) { 'gateway_rejected' } else { 'gateway_transport_unknown' } }
    $requestId = [string](Get-PropertyValue $parsed 'request_id')
    if (-not $requestId) { $requestId = [string](Get-PropertyValue $errorValue 'request_id') }
    if ($requestId -cnotmatch '^(request[_-]|req[_-])[A-Za-z0-9_-]{1,128}$') { $requestId = '' }
    $safe = [ordered]@{ http_status = $status; error_code = $code; request_id = $requestId }
    $message = "AI Work 网关请求失败：HTTP $status；error_code=$code"
    if ($requestId) { $message += "；request_id=$requestId" }
    $exception = [Exception]::new($message)
    $exception.Data['AiworkSafeError'] = ($safe | ConvertTo-Json -Compress)
    return $exception
}

function Send-AiworkPreparedSubmission($Payload, [string[]]$ImageIds, [string[]]$ImagePaths, [string[]]$VideoIds, [string[]]$VideoPaths, [string]$Key) {
    $orderedPayload = [ordered]@{}
    foreach ($name in @($Payload.Keys | Sort-Object)) { $orderedPayload[$name] = $Payload[$name] }
    $identity = [ordered]@{ payload = $orderedPayload; imageIds = @($ImageIds); imagePaths = @($ImagePaths); videoIds = @($VideoIds); videoPaths = @($VideoPaths) }
    $fingerprint = Get-AiworkSha256 ([Text.Encoding]::UTF8.GetBytes(($identity | ConvertTo-Json -Depth 20 -Compress)))
    $directory = Get-AiworkReferenceDirectory
    $name = Get-AiworkSha256 ([Text.Encoding]::UTF8.GetBytes($Key))
    $journalPath = Join-Path $directory "submission-$name.json"
    $record = [ordered]@{ schemaVersion = 1; fingerprint = $fingerprint; phase = 'preparing' }
    # A durable create-new claim stops another process before any upload or paid POST.
    try { $claim = [IO.File]::Open($journalPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None) }
    catch [IO.IOException] {
        $prior = $null
        try { $prior = Read-AiworkReferenceJson $journalPath } catch { throw 'SUBMISSION_IN_PROGRESS: 同一提交键正在处理。' }
        if (-not $prior) { throw 'SUBMISSION_RECORD_UNAVAILABLE' }
        if ($prior.fingerprint -cne $fingerprint) { throw 'SUBMISSION_IDENTITY_CONFLICT: 同一提交键对应的输入已变化。' }
        if ($prior.phase -eq 'submitted') { return $prior.result }
        if ($prior.phase -eq 'preparing') { throw 'SUBMISSION_IN_PROGRESS: 同一提交键正在准备，不能重复提交。' }
        throw 'SUBMISSION_UNKNOWN: 此提交键已有失败或结果未知记录，请核对原请求，不能自动重提。'
    }
    try {
        $encoded = [Text.Encoding]::UTF8.GetBytes(($record | ConvertTo-Json -Compress))
        $claim.Write($encoded, 0, $encoded.Length); $claim.Flush($true)
    } finally { $claim.Dispose() }
    try {
        $images = @(); $videos = @()
        foreach ($id in $ImageIds) { $images += Resolve-AiworkReferenceSource $id 'image' }
        foreach ($path in $ImagePaths) { $images += Get-AiworkReferenceFile $path }
        foreach ($id in $VideoIds) { $videos += Resolve-AiworkReferenceSource $id 'video' }
        foreach ($path in $VideoPaths) { $videos += Get-AiworkReferenceFile $path }
        if ($images.Count -gt 9 -or $videos.Count -gt 10) { throw 'REFERENCE_LIMIT_EXCEEDED' }
        foreach ($file in $images) { if (-not $file.mime.StartsWith('image/')) { throw 'REFERENCE_MEDIA_MISMATCH' } }
        foreach ($file in $videos) { if (-not $file.mime.StartsWith('video/')) { throw 'REFERENCE_MEDIA_MISMATCH' } }
        # Every independent task uploads fresh bytes. Original ID aliases keep their
        # bound files and order; no live task is replayed to obtain fresh references.
        $freshImages = @(); $freshVideos = @()
        foreach ($file in $images) { $freshImages += Upload-Asset $file.path $file.sha256 }
        foreach ($file in $videos) { $freshVideos += Upload-Asset $file.path $file.sha256 }
        if ($freshImages.Count) { $Payload.image_asset_ids = @($freshImages) }
        if ($freshVideos.Count) { $Payload.video_asset_ids = @($freshVideos) }
        $record.phase = 'sending'; Write-AiworkReferenceJson $journalPath $record
        $response = Invoke-AiworkJson -Method POST -Uri (Get-ApiUri '/videos/generations') -Body $Payload -ExtraHeaders @{ 'Idempotency-Key' = $Key }
        $id = Get-TaskId $response
        if ([string]::IsNullOrWhiteSpace($id)) { throw 'SUBMISSION_UNKNOWN: 网关未返回任务 ID，不能自动重提。' }
        $result = [ordered]@{ task_id = $id; status = 'submitted'; idempotency_key = $Key; work_context = (Get-PropertyValue $response 'work_context') }
        $record.phase = 'submitted'; $record.result = $result
        Write-AiworkReferenceJson $journalPath $record
        return [pscustomobject]$result
    } catch {
        $record.phase = if ($record.phase -eq 'preparing') { 'preparation_failed' } else { 'unknown' }
        try { Write-AiworkReferenceJson $journalPath $record } catch {}
        throw
    }
}
