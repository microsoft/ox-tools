# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ErrorActionPreference = 'Stop'
"Anvil end-of-job disk usage (logical file sizes; GiB = 2^30 bytes)"
if ($IsWindows) {
    Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' |
        Select-Object DeviceID,
            @{Name='SizeGiB'; Expression={[math]::Round($_.Size / 1GB, 2)}},
            @{Name='UsedGiB'; Expression={[math]::Round(($_.Size - $_.FreeSpace) / 1GB, 2)}},
            @{Name='FreeGiB'; Expression={[math]::Round($_.FreeSpace / 1GB, 2)}} |
        Format-Table -AutoSize | Out-String -Width 200 | Write-Output
} else {
    & df -h
    if ($LASTEXITCODE -ne 0) { Write-Warning "df failed with exit code $LASTEXITCODE" }
}

$workspace = (Get-Location).Path
$target = Join-Path $workspace 'target'
$paths = @($workspace, $target, (Join-Path $HOME '.cargo'), (Join-Path $HOME '.rustup'))
$paths += Join-Path $target 'debug/incremental'
if (Test-Path -LiteralPath $target -PathType Container) {
    $paths += @(Get-ChildItem -LiteralPath $target -Directory -Force | ForEach-Object FullName)
    $coverage = Join-Path $target 'coverage-gate'
    if (Test-Path -LiteralPath $coverage -PathType Container) {
        foreach ($run in Get-ChildItem -LiteralPath $coverage -Directory) {
            $cargoTarget = Join-Path $run.FullName 'cargo-target'
            if (Test-Path -LiteralPath $cargoTarget -PathType Container) {
                foreach ($configuration in Get-ChildItem -LiteralPath $cargoTarget -Directory) {
                    $paths += $configuration.FullName
                    $paths += Join-Path $configuration.FullName 'debug/incremental'
                    $paths += Join-Path $configuration.FullName 'tests/trybuild'
                }
            }
        }
    }
}
"Directory totals overlap; parent and child rows must not be added together."
foreach ($path in $paths | Select-Object -Unique) {
    if (-not (Test-Path -LiteralPath $path -PathType Container)) {
        "Not present: $path"
        continue
    }
    try {
        if ($IsLinux) {
            $usage = & du -sb -- $path
            if ($LASTEXITCODE -ne 0) { throw "du failed with exit code $LASTEXITCODE" }
            $bytes = [long](($usage -split '\s+', 2)[0])
        } else {
            $bytes = (Get-ChildItem -LiteralPath $path -File -Recurse -Force |
                Measure-Object -Property Length -Sum).Sum
        }
        "{0:N2} GiB`t{1}" -f ($bytes / 1GB), $path
    } catch {
        Write-Warning "Could not measure '$path': $_"
    }
}
