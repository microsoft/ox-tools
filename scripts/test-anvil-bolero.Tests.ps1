# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Exercise the emitted recipe's selection boundary without processes or disk fixtures.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$template = Get-Content -Raw (Join-Path $PSScriptRoot '..\crates\cargo-anvil\templates\justfiles\anvil\checks\bolero.just')
$selection = [regex]::Match($template, '(?ms)^    function Get-AnvilBoleroPackages\b.*?^    }')
if (-not $selection.Success) { throw 'Missing Bolero candidate selector in the source template' }
. ([scriptblock]::Create($selection.Value))

function Assert-Packages($metadata, [string]$include, [string[]]$expected) {
    $actual = @(Get-AnvilBoleroPackages $metadata $include)
    $actualJson = ConvertTo-Json -InputObject $actual -Compress
    $expectedJson = ConvertTo-Json -InputObject @($expected) -Compress
    if ($actualJson -cne $expectedJson) {
        throw "Selection '$include': expected $expectedJson, got $actualJson"
    }
}

function Assert-Rejected($metadata, [string]$include, [string]$message) {
    $failure = $null
    try { Get-AnvilBoleroPackages $metadata $include | Out-Null } catch { $failure = $_.Exception.Message }
    if (-not $failure -or -not $failure.Contains($message)) {
        throw "Expected rejection containing '$message', got '$failure'"
    }
}

function New-Metadata {
    # External package collides with the no-target member by name AND version.
    # Resolved IDs, not names or dependency aliases, establish graph edges.
    @'
{
  "workspace_members": ["path#plain@1.0.0", "path#unit@1.0.0", "path#integration@1.0.0", "path#optional@1.0.0"],
  "packages": [
    {"id":"path#plain@1.0.0", "name":"plain", "version":"1.0.0"},
    {"id":"path#unit@1.0.0", "name":"unit", "version":"1.0.0"},
    {"id":"path#integration@1.0.0", "name":"integration", "version":"1.0.0"},
    {"id":"path#optional@1.0.0", "name":"optional", "version":"1.0.0"},
    {"id":"registry#plain@1.0.0", "name":"plain", "version":"1.0.0"},
    {"id":"registry#adapter@1.0.0", "name":"adapter", "version":"1.0.0"},
    {"id":"registry#bolero@0.13.4", "name":"bolero", "version":"0.13.4"},
    {"id":"registry#bolero-engine@0.13.4", "name":"bolero-engine", "version":"0.13.4"}
  ],
  "resolve": {"nodes": [
    {"id":"path#plain@1.0.0", "dependencies":[]},
    {"id":"path#unit@1.0.0", "dependencies":["registry#bolero@0.13.4"]},
    {"id":"path#integration@1.0.0", "dependencies":["registry#adapter@1.0.0"]},
    {"id":"path#optional@1.0.0", "dependencies":["registry#bolero-engine@0.13.4"]},
    {"id":"registry#plain@1.0.0", "dependencies":["registry#bolero@0.13.4"]},
    {"id":"registry#adapter@1.0.0", "dependencies":["registry#bolero@0.13.4"]},
    {"id":"registry#bolero@0.13.4", "dependencies":["registry#bolero-engine@0.13.4"]},
    {"id":"registry#bolero-engine@0.13.4", "dependencies":[]}
  ]}
}
'@ | ConvertFrom-Json
}

# No source layout or target-name filtering: direct unit, re-exported integration,
# and optional/target-specific engine edges are all conservative candidates.
Assert-Packages (New-Metadata) '--workspace' @('unit@1.0.0', 'integration@1.0.0', 'optional@1.0.0')
Assert-Packages (New-Metadata) '' @('unit@1.0.0', 'integration@1.0.0', 'optional@1.0.0')
Assert-Packages (New-Metadata) '--package plain@1.0.0' @()
Assert-Packages (New-Metadata) '--package integration@1.0.0 --package plain@1.0.0' @('integration@1.0.0')
Assert-Packages (New-Metadata) '--package unit@1.0.0 --package unit@1.0.0' @('unit@1.0.0')

$metadata = New-Metadata
$metadata.packages = @($metadata.packages | Where-Object { $_.name -notin @('bolero', 'bolero-engine') })
$metadata.resolve.nodes = @($metadata.resolve.nodes | Where-Object { $_.id -notmatch '#bolero' })
foreach ($node in $metadata.resolve.nodes) { $node.dependencies = @() }
Assert-Packages $metadata '--workspace' @()

# Graph ordering and cycles must not affect reachability or duplicate candidates.
$metadata = New-Metadata
$metadata.resolve.nodes[5].dependencies += 'path#integration@1.0.0'
[array]::Reverse($metadata.resolve.nodes)
Assert-Packages $metadata '--workspace' @('unit@1.0.0', 'integration@1.0.0', 'optional@1.0.0')

Assert-Rejected (New-Metadata) '--package missing@1.0.0' 'not a workspace member'
Assert-Rejected (New-Metadata) '--package' 'unsupported affected-package arguments'
Assert-Rejected (New-Metadata) '--exclude plain@1.0.0' 'unsupported affected-package arguments'
Assert-Rejected ([pscustomobject]@{ packages = @() }) '--workspace' 'packages, workspace_members, and resolve.nodes'
$metadata = New-Metadata
$metadata.resolve = $null
Assert-Rejected $metadata '--workspace' 'packages, workspace_members, and resolve.nodes'
$metadata = New-Metadata
$metadata.resolve.nodes[0].dependencies = @('missing-id')
Assert-Rejected $metadata '--workspace' "missing dependency node 'missing-id'"
$metadata = New-Metadata
$metadata.resolve.nodes[0].dependencies = $null
Assert-Rejected $metadata '--workspace' 'missing its id or dependencies'
$metadata = New-Metadata
$metadata.workspace_members += 'missing-member'
Assert-Rejected $metadata '--workspace' 'missing a workspace member'
$metadata = New-Metadata
$metadata.packages[0].id = 'missing-node'
Assert-Rejected $metadata '--workspace' 'missing its identity or resolve node'

Write-Host 'PASS: Bolero build-free candidate selection (workspace, impact, identity, reachability, and invalid metadata)'
