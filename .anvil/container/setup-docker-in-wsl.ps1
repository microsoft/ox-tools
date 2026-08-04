# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

#Requires -Version 5.1

[CmdletBinding()]
param(
    [switch]$Doctor,
    [string]$Distro,
    [switch]$Yes,
    [Parameter(DontShow = $true)]
    [string]$FactsPath
)

$ErrorActionPreference = 'Stop'
$minimumWindowsBuild = 22000
$minimumWslVersion = [version]'2.1.0'
$minimumDockerVersion = [version]'23.0.0'
$supportedUbuntuVersions = @('22.04', '24.04')

function ConvertTo-Version([string]$Value, [string]$Name) {
    $match = [regex]::Match($Value, '(\d+)\.(\d+)(?:\.(\d+))?')
    if (-not $match.Success) {
        throw "docker-in-wsl: could not parse $Name version '$Value'."
    }
    [version]::new(
        [int]$match.Groups[1].Value,
        [int]$match.Groups[2].Value,
        $(if ($match.Groups[3].Success) { [int]$match.Groups[3].Value } else { 0 })
    )
}

function Invoke-WslScript([string]$Distribution, [string]$Script, [switch]$Root) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($Script.Replace("`r", ''))
    $encoded = [Convert]::ToBase64String($bytes)
    $arguments = @('-d', $Distribution)
    if ($Root) {
        $arguments += @('--user', 'root')
    }
    $arguments += @('--', 'bash', '-lc', "echo '$encoded' | base64 -d | bash")
    $output = & wsl.exe @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "docker-in-wsl: command failed in WSL distribution '$Distribution' with exit code $LASTEXITCODE."
    }
    @($output)
}

function Get-WslRegistration([string]$RequestedDistro) {
    $root = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss'
    if (-not (Test-Path $root)) {
        throw 'docker-in-wsl: no WSL distribution is registered. Install Ubuntu 22.04 or 24.04 with `wsl --install -d Ubuntu-24.04`.'
    }

    $registrations = @(Get-ChildItem $root | ForEach-Object {
        $properties = Get-ItemProperty $_.PSPath
        [pscustomobject]@{
            Id = $_.PSChildName
            Name = [string]$properties.DistributionName
            Version = [int]$properties.Version
        }
    })
    if ($RequestedDistro) {
        $registration = $registrations | Where-Object Name -eq $RequestedDistro | Select-Object -First 1
        if (-not $registration) {
            throw "docker-in-wsl: WSL distribution '$RequestedDistro' is not registered."
        }
        return $registration
    }

    $defaultId = [string](Get-ItemProperty $root).DefaultDistribution
    $registration = $registrations | Where-Object Id -eq $defaultId | Select-Object -First 1
    if (-not $registration) {
        throw 'docker-in-wsl: WSL has no default distribution. Select one with `wsl --set-default <name>` or pass `-Distro <name>`.'
    }
    $registration
}

function ConvertFrom-KeyValue([string[]]$Lines) {
    $values = @{}
    foreach ($line in $Lines) {
        $separator = $line.IndexOf('=')
        if ($separator -gt 0) {
            $values[$line.Substring(0, $separator)] = $line.Substring($separator + 1)
        }
    }
    $values
}

function Get-LiveFacts {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        throw 'docker-in-wsl: WSL is not installed. Run `wsl --install -d Ubuntu-24.04` from an elevated terminal, restart Windows, then rerun.'
    }

    $registration = Get-WslRegistration $Distro
    $wslVersionOutput = ((& wsl.exe --version 2>$null) -join "`n").Replace("`0", '')
    if ($LASTEXITCODE -ne 0 -or -not $wslVersionOutput) {
        throw 'docker-in-wsl: the Microsoft Store version of WSL is required. Run `wsl --update`, then rerun.'
    }
    $wslVersion = ConvertTo-Version $wslVersionOutput 'WSL'

    $probe = @'
set -eu
. /etc/os-release
unset DOCKER_CONTEXT DOCKER_TLS_VERIFY DOCKER_CERT_PATH
export DOCKER_HOST=unix:///var/run/docker.sock
docker_path="$(command -v docker 2>/dev/null || true)"
docker_real_path="$(readlink -f "$docker_path" 2>/dev/null || true)"
docker_client_version="$(docker version --format '{{.Client.Version}}' 2>/dev/null || true)"
docker_server_version="$(docker version --format '{{.Server.Version}}' 2>/dev/null || true)"
conflicting_packages="$(dpkg-query -W -f='${db:Status-Abbrev} ${binary:Package}\n' docker.io docker-compose docker-compose-v2 docker-doc podman-docker containerd runc 2>/dev/null | awk '$1 ~ /^ii/ { print $2 }' | sort -u | tr '\n' ' ' | sed 's/ $//' || true)"
printf 'user=%s\n' "$(id -un)"
printf 'os_id=%s\n' "${ID:-}"
printf 'os_version=%s\n' "${VERSION_ID:-}"
printf 'systemd_configured=%s\n' "$(grep -Eq '^[[:space:]]*systemd[[:space:]]*=[[:space:]]*true[[:space:]]*$' /etc/wsl.conf 2>/dev/null && echo true || echo false)"
printf 'systemd_running=%s\n' "$([ -d /run/systemd/system ] && echo true || echo false)"
printf 'docker_path=%s\n' "$docker_path"
printf 'docker_real_path=%s\n' "$docker_real_path"
printf 'docker_client_version=%s\n' "$docker_client_version"
printf 'docker_server_version=%s\n' "$docker_server_version"
printf 'conflicting_packages=%s\n' "$conflicting_packages"
printf 'docker_service_installed=%s\n' "$(systemctl cat docker.service >/dev/null 2>&1 && echo true || echo false)"
printf 'docker_service_enabled=%s\n' "$(systemctl is-enabled docker 2>/dev/null | grep -qx enabled && echo true || echo false)"
printf 'docker_service_active=%s\n' "$(systemctl is-active docker 2>/dev/null | grep -qx active && echo true || echo false)"
printf 'user_in_docker_group=%s\n' "$(id -nG | tr ' ' '\n' | grep -qx docker && echo true || echo false)"
printf 'docker_socket_access=%s\n' "$(docker version >/dev/null 2>&1 && echo true || echo false)"
'@
    $values = ConvertFrom-KeyValue (Invoke-WslScript $registration.Name $probe)
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $bridgeVersion = & wsl.exe -d $registration.Name -- env `
            -u DOCKER_CONTEXT `
            -u DOCKER_TLS_VERIFY `
            -u DOCKER_CERT_PATH `
            'DOCKER_HOST=unix:///var/run/docker.sock' `
            docker version --format '{{.Server.Version}}' 2>$null
        $windowsBridge = $LASTEXITCODE -eq 0 -and [bool]$bridgeVersion
    } finally {
        $ErrorActionPreference = $previousErrorAction
    }
    $desktopDistros = @(Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss' | ForEach-Object {
        [string](Get-ItemProperty $_.PSPath).DistributionName
    } | Where-Object { $_ -like 'docker-desktop*' })

    [pscustomobject]@{
        windowsBuild = [Environment]::OSVersion.Version.Build
        wslVersion = $wslVersion.ToString()
        distro = $registration.Name
        wslVersionMode = $registration.Version
        osId = $values.os_id
        osVersion = $values.os_version
        systemdConfigured = $values.systemd_configured -eq 'true'
        systemdRunning = $values.systemd_running -eq 'true'
        dockerPath = $values.docker_path
        dockerRealPath = $values.docker_real_path
        dockerClientVersion = $values.docker_client_version
        dockerServerVersion = $values.docker_server_version
        conflictingPackages = $values.conflicting_packages
        dockerServiceInstalled = $values.docker_service_installed -eq 'true'
        dockerServiceEnabled = $values.docker_service_enabled -eq 'true'
        dockerServiceActive = $values.docker_service_active -eq 'true'
        user = $values.user
        userInDockerGroup = $values.user_in_docker_group -eq 'true'
        dockerSocketAccess = $values.docker_socket_access -eq 'true'
        windowsBridge = $windowsBridge
        dockerDesktopDistros = $desktopDistros
    }
}

function Get-Facts {
    if ($FactsPath) {
        if (-not $Doctor) {
            throw 'docker-in-wsl: -FactsPath is accepted only with -Doctor and never performs host changes.'
        }
        return Get-Content -Raw -LiteralPath $FactsPath | ConvertFrom-Json
    }
    Get-LiveFacts
}

function Test-DesktopInjection($Facts) {
    [string]$Facts.dockerRealPath -like '/mnt/wsl/docker-desktop*'
}

function Write-Check([bool]$Passed, [string]$Name, [string]$Detail, [switch]$Warning) {
    $label = if ($Passed) { 'OK' } elseif ($Warning) { 'WARN' } else { 'FAIL' }
    Write-Host ("[{0}] {1}: {2}" -f $label, $Name, $Detail)
}

function Show-Doctor($Facts) {
    $ready = $true
    $windowsSupported = [int]$Facts.windowsBuild -ge $minimumWindowsBuild
    Write-Check $windowsSupported 'Windows' "build $($Facts.windowsBuild); Windows 11 build $minimumWindowsBuild or newer is supported"
    $ready = $ready -and $windowsSupported

    $parsedWslVersion = ConvertTo-Version ([string]$Facts.wslVersion) 'WSL'
    $wslSupported = $parsedWslVersion -ge $minimumWslVersion
    Write-Check $wslSupported 'WSL' "version $parsedWslVersion; $minimumWslVersion or newer is supported"
    $ready = $ready -and $wslSupported

    $wsl2 = [int]$Facts.wslVersionMode -eq 2
    Write-Check $wsl2 'Distribution' "$($Facts.distro) is WSL $($Facts.wslVersionMode); run `wsl --set-version $($Facts.distro) 2` when this is not WSL 2"
    $ready = $ready -and $wsl2

    $distributionSupported = $Facts.osId -eq 'ubuntu' -and $Facts.osVersion -in $supportedUbuntuVersions
    Write-Check $distributionSupported 'Linux' "$($Facts.osId) $($Facts.osVersion); supported distributions are Ubuntu $($supportedUbuntuVersions -join ' and ')"
    $ready = $ready -and $distributionSupported

    Write-Check ([bool]$Facts.systemdConfigured) 'systemd configuration' 'systemd=true in /etc/wsl.conf'
    Write-Check ([bool]$Facts.systemdRunning) 'systemd runtime' 'systemd is the active WSL init system'
    $ready = $ready -and [bool]$Facts.systemdConfigured -and [bool]$Facts.systemdRunning

    $desktopInjection = Test-DesktopInjection $Facts
    if ($desktopInjection) {
        Write-Check $false 'Docker CLI' "resolves through Docker Desktop at $($Facts.dockerRealPath); remove the injected link or restore a native Docker Engine installation manually"
        $ready = $false
    } elseif (-not $Facts.dockerPath) {
        Write-Check $false 'Docker CLI' 'not installed; run this script without -Doctor to install Docker Engine'
        $ready = $false
    } else {
        Write-Check $true 'Docker CLI' "$($Facts.dockerRealPath)"
    }

    $dockerVersionSupported = $false
    if ($Facts.dockerServerVersion) {
        $dockerVersionSupported = (ConvertTo-Version ([string]$Facts.dockerServerVersion) 'Docker Engine') -ge $minimumDockerVersion
    }
    Write-Check $dockerVersionSupported 'Docker Engine' "$(if ($Facts.dockerServerVersion) { $Facts.dockerServerVersion } else { 'unavailable' }); $minimumDockerVersion or newer is required"
    $ready = $ready -and $dockerVersionSupported

    Write-Check ([bool]$Facts.dockerServiceInstalled) 'Docker service' 'docker.service is installed'
    Write-Check ([bool]$Facts.dockerServiceEnabled) 'Docker startup' 'docker.service is enabled'
    Write-Check ([bool]$Facts.dockerServiceActive) 'Docker daemon' 'docker.service is active'
    Write-Check ([bool]$Facts.userInDockerGroup) 'Docker group' "$($Facts.user) belongs to the docker group"
    Write-Check ([bool]$Facts.dockerSocketAccess) 'Docker socket' "$($Facts.user) can query the daemon without sudo"
    Write-Check ([bool]$Facts.windowsBridge) 'Windows bridge' "Windows reaches the daemon with wsl.exe -d $($Facts.distro) -- docker version"
    $ready = $ready -and [bool]$Facts.dockerServiceInstalled
    $ready = $ready -and [bool]$Facts.dockerServiceEnabled -and [bool]$Facts.dockerServiceActive
    $ready = $ready -and [bool]$Facts.userInDockerGroup -and [bool]$Facts.dockerSocketAccess -and [bool]$Facts.windowsBridge

    if ($desktopInjection) {
        Write-Check $false 'Docker Desktop' 'conflicting injection detected; Docker Desktop and its data are never removed by this script'
    } elseif (@($Facts.dockerDesktopDistros).Count -gt 0) {
        Write-Check $false 'Docker Desktop' 'detected but not used; it is never removed by this script' -Warning
    } else {
        Write-Check $true 'Docker Desktop' 'no conflicting Docker Desktop injection detected'
    }
    if ($Facts.conflictingPackages) {
        Write-Check $false 'Docker packages' "$($Facts.conflictingPackages) are preserved while the existing Engine works; bootstrap replaces them only when an install or upgrade is required" -Warning
    } else {
        Write-Check $true 'Docker packages' 'no packages conflict with Docker CE installation'
    }

    if ($ready) {
        Write-Host '[READY] Docker Engine in WSL satisfies the Anvil host contract.'
    } else {
        Write-Host '[ACTION] Run this script without -Doctor to install or repair non-destructive prerequisites.'
    }
    $ready
}

function Confirm-Actions([string[]]$Actions) {
    Write-Output 'docker-in-wsl will:'
    foreach ($action in $Actions) {
        Write-Output "  - $action"
    }
    Write-Output 'Docker Desktop, WSL distributions, containers, images, and volumes will not be removed.'
    if ($Yes) {
        return
    }
    if (-not [Environment]::UserInteractive -or [Console]::IsInputRedirected) {
        throw 'docker-in-wsl: confirmation is required. Rerun with -Yes after reviewing the planned changes.'
    }
    if ((Read-Host 'Continue? [y/N]') -notmatch '^[Yy]$') {
        throw 'docker-in-wsl: cancelled.'
    }
}

function Enable-Systemd([string]$Distribution) {
    $script = @'
set -eu
tmp="$(mktemp)"
awk '
BEGIN { in_boot=0; saw_boot=0; set_systemd=0 }
/^[[:space:]]*\[boot\][[:space:]]*$/ {
    in_boot=1
    saw_boot=1
    print
    next
}
/^[[:space:]]*\[/ {
    if (in_boot && !set_systemd) {
        print "systemd=true"
        set_systemd=1
    }
    in_boot=0
}
in_boot && /^[[:space:]]*systemd[[:space:]]*=/ {
    if (!set_systemd) {
        print "systemd=true"
        set_systemd=1
    }
    next
}
{ print }
END {
    if (!saw_boot) {
        print ""
        print "[boot]"
        print "systemd=true"
    } else if (in_boot && !set_systemd) {
        print "systemd=true"
    }
}
' /etc/wsl.conf 2>/dev/null >"$tmp" || {
    printf '[boot]\nsystemd=true\n' >"$tmp"
}
install -m 0644 "$tmp" /etc/wsl.conf
rm -f "$tmp"
'@
    Invoke-WslScript $Distribution $script -Root | Out-Null
}

function Install-Docker([string]$Distribution) {
    $script = @'
set -eu
. /etc/os-release
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl
apt-get remove -y docker.io docker-compose docker-compose-v2 docker-doc podman-docker containerd runc || true
install -m 0755 -d /etc/apt/keyrings
curl -fsSL "https://download.docker.com/linux/ubuntu/gpg" -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
arch="$(dpkg --print-architecture)"
printf 'Types: deb\nURIs: https://download.docker.com/linux/ubuntu\nSuites: %s\nComponents: stable\nArchitectures: %s\nSigned-By: /etc/apt/keyrings/docker.asc\n' \
    "$VERSION_CODENAME" "$arch" >/etc/apt/sources.list.d/docker.sources
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
'@
    Invoke-WslScript $Distribution $script -Root | Out-Null
}

function Configure-Docker([string]$Distribution, [string]$User) {
    if ($User -notmatch '^[a-z_][a-z0-9_-]*[$]?$') {
        throw "docker-in-wsl: unsupported Linux user name '$User'."
    }
    $script = @"
set -eu
systemctl enable --now docker
groupadd -f docker
usermod -aG docker -- '$User'
"@
    Invoke-WslScript $Distribution $script -Root | Out-Null
}

$facts = Get-Facts
if ($Doctor) {
    if (Show-Doctor $facts) {
        exit 0
    }
    exit 1
}

if ([int]$facts.windowsBuild -lt $minimumWindowsBuild) {
    throw "docker-in-wsl: Windows 11 build $minimumWindowsBuild or newer is required."
}
if ((ConvertTo-Version ([string]$facts.wslVersion) 'WSL') -lt $minimumWslVersion) {
    throw "docker-in-wsl: WSL $minimumWslVersion or newer is required. Run `wsl --update`."
}
if ([int]$facts.wslVersionMode -ne 2) {
    throw "docker-in-wsl: '$($facts.distro)' must use WSL 2. Run `wsl --set-version $($facts.distro) 2`."
}
if ($facts.osId -ne 'ubuntu' -or $facts.osVersion -notin $supportedUbuntuVersions) {
    throw "docker-in-wsl: supported distributions are Ubuntu $($supportedUbuntuVersions -join ' and '); found $($facts.osId) $($facts.osVersion)."
}
if (Test-DesktopInjection $facts) {
    throw "docker-in-wsl: Docker resolves through Docker Desktop at $($facts.dockerRealPath). This script does not remove Docker Desktop or its data. Follow .anvil/container/README.md to remove the injected link manually, then rerun."
}

$actions = @()
$restartForSystemd = -not $facts.systemdConfigured -or -not $facts.systemdRunning
$installDocker = -not $facts.dockerPath -or -not $facts.dockerServiceInstalled
if ($facts.dockerClientVersion) {
    $installDocker = $installDocker -or (ConvertTo-Version ([string]$facts.dockerClientVersion) 'Docker client') -lt $minimumDockerVersion
}
if ($facts.dockerServerVersion) {
    $installDocker = $installDocker -or (ConvertTo-Version ([string]$facts.dockerServerVersion) 'Docker Engine') -lt $minimumDockerVersion
}
if ($restartForSystemd) {
    $actions += "enable systemd in $($facts.distro) and terminate only that distribution once"
}
if ($installDocker) {
    if ($facts.conflictingPackages) {
        $actions += "replace conflicting packages ($($facts.conflictingPackages)); Docker data under /var/lib/docker is preserved"
    }
    $actions += 'add the official Docker apt repository and install or update Docker Engine, CLI, Buildx, and Compose'
}
if (-not $facts.dockerServiceEnabled -or -not $facts.dockerServiceActive) {
    $actions += 'enable and start docker.service'
}
if (-not $facts.userInDockerGroup) {
    $actions += "add $($facts.user) to the docker group and terminate only $($facts.distro) once to refresh group membership"
}
if ($actions.Count -eq 0) {
    Write-Output 'docker-in-wsl: no changes are required.'
    if (Show-Doctor $facts) { exit 0 }
    exit 1
}

Confirm-Actions $actions
if ($restartForSystemd) {
    Enable-Systemd $facts.distro
    & wsl.exe --terminate $facts.distro
    if ($LASTEXITCODE -ne 0) {
        throw "docker-in-wsl: could not terminate '$($facts.distro)' to activate systemd."
    }
    Start-Sleep -Seconds 2
    Invoke-WslScript $facts.distro 'true' | Out-Null
}
if ($installDocker) {
    Install-Docker $facts.distro
}
Configure-Docker $facts.distro $facts.user

if (-not $facts.userInDockerGroup) {
    & wsl.exe --terminate $facts.distro
    if ($LASTEXITCODE -ne 0) {
        throw "docker-in-wsl: could not terminate '$($facts.distro)' to refresh docker group membership."
    }
    Start-Sleep -Seconds 2
    Invoke-WslScript $facts.distro 'true' | Out-Null
}

$facts = Get-LiveFacts
if (Show-Doctor $facts) {
    exit 0
}
throw 'docker-in-wsl: bootstrap completed, but validation still reports failures. Use the diagnostics above for remediation.'
