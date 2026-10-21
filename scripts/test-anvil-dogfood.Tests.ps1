# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tokens = $null
$parseErrors = $null
$scriptPath = Join-Path $PSScriptRoot 'test-anvil-dogfood.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw ($parseErrors -join "`n") }
$function = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Get-ContainerArguments'
}, $true)
if ($null -eq $function) { throw 'Missing in-process container argument builder' }
. ([scriptblock]::Create($function.Extent.Text))

$previousToken = [Environment]::GetEnvironmentVariable('GITHUB_TOKEN')
try {
    foreach ($token in @($null, '', ' ', 'dogfood-test-token')) {
        [Environment]::SetEnvironmentVariable('GITHUB_TOKEN', $token)
        $actual = @(Get-ContainerArguments)
        if ((ConvertTo-Json -InputObject $actual -Compress) -cne '["anvil-container"]') {
            throw 'Default invocation must not opt in, even when a token is supplied'
        }

        if ([string]::IsNullOrWhiteSpace($token)) {
            $failure = $null
            try { Get-ContainerArguments -GitHubToken | Out-Null } catch { $failure = $_.Exception.Message }
            if ($failure -cne '-GitHubToken requires a supplied GITHUB_TOKEN in the host environment.') {
                throw 'Explicit authentication without a usable token must fail'
            }
        } else {
            $actual = @(Get-ContainerArguments -GitHubToken)
            if ((ConvertTo-Json -InputObject $actual -Compress) -cne '["anvil-container","--github-token"]') {
                throw 'Opt-in must pass only the launcher flag, never the token value'
            }
        }
    }
} finally {
    [Environment]::SetEnvironmentVariable('GITHUB_TOKEN', $previousToken)
}

Write-Host 'PASS: dogfood container authentication argument contract'
