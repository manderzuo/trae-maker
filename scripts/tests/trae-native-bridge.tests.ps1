$ErrorActionPreference = 'Stop'
$bridge = Join-Path $PSScriptRoot '../../src-ps/trae-switch-bridge.ps1'
$tokens = $null; $errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($bridge, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Bridge has parse errors' }
foreach ($name in @('Test-DefaultTraeProcess', 'Get-TraeLaunchArguments')) {
    $fn = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
    if (-not $fn) { throw "Missing native bridge behavior: $name" }
    Invoke-Expression $fn.Extent.Text
}
$expected = 'E:\apps\Trae CN.exe'
$root = [pscustomobject]@{ ExecutablePath=$expected; CommandLine='"E:\apps\Trae CN.exe" --remote-debugging-port=9333'; ProcessId=42 }
if (-not (Test-DefaultTraeProcess $root $expected)) { throw 'Default root rejected' }
foreach ($argsLine in @('--user-data-dir=E:\isolated', '--user-data-dir E:\isolated', '--type=renderer')) {
    $other = [pscustomobject]@{ ExecutablePath=$expected; CommandLine=('"E:\apps\Trae CN.exe" ' + $argsLine); ProcessId=43 }
    if (Test-DefaultTraeProcess $other $expected) { throw "Independent process included: $argsLine" }
}
$Script:TargetApp = 'Trae'; $Script:LaunchProxyPort=8899; $Script:LaunchCdpPort=9333
$argsList = @(Get-TraeLaunchArguments)
foreach ($arg in @('--proxy-server=http://127.0.0.1:8899','--remote-debugging-port=9333','--remote-debugging-address=127.0.0.1')) {
    if ($argsList -notcontains $arg) { throw "Restart lost argument: $arg" }
}
Write-Output 'Native bridge: selected default process and restart arguments PASS'
