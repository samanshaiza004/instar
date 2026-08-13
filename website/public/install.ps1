# Stable website entry point for the cargo-dist generated Instar installer on
# Windows. Mirrors public/install.sh: this script always delegates to whatever
# the latest GitHub release generated, so a documented URL survives every tag.
#
#   irm https://instar.samanshaiza.com/install.ps1 | iex

$ErrorActionPreference = 'Stop'

$Repository = 'samanshaiza004/instar'
$Installer = 'instar-shell-installer.ps1'
$BaseUrl = if ($env:INSTAR_INSTALLER_GITHUB_BASE_URL) {
  $env:INSTAR_INSTALLER_GITHUB_BASE_URL
} else {
  'https://github.com'
}
$InstallerUrl = "$BaseUrl/$Repository/releases/latest/download/$Installer"
$Site = 'https://instar.samanshaiza.com'

function Write-Note($Message) {
  Write-Host "instar installer: $Message"
}

if ($PSVersionTable.PSVersion.Major -lt 5) {
  throw "instar installer: PowerShell 5 or newer is required."
}

# TLS 1.2 is not the default on older Windows PowerShell hosts.
try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
  # PowerShell 7+ manages this itself; nothing to do.
}

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("instar-install-" + [Guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $TempDir | Out-Null
$ScriptPath = Join-Path $TempDir $Installer

try {
  Write-Note 'fetching the latest release installer...'
  try {
    Invoke-WebRequest -Uri $InstallerUrl -OutFile $ScriptPath -UseBasicParsing
  } catch {
    $status = $null
    if ($_.Exception.Response) {
      $status = [int]$_.Exception.Response.StatusCode
    }

    if ($status -eq 404) {
      Write-Host ''
      Write-Note 'no tagged Instar binary release exists yet.'
      Write-Host 'Build the current developer preview from source:'
      Write-Host ''
      Write-Host "  git clone https://github.com/$Repository.git"
      Write-Host '  cd instar'
      Write-Host '  cargo install --locked --path crates/instar-shell'
      Write-Host ''
      Write-Host "Guide: $Site/docs/development/build-from-source/"
      exit 1
    }

    Write-Note "could not reach GitHub Releases ($InstallerUrl)."
    Write-Host 'Check your network, then retry.'
    exit 1
  }

  Write-Note 'handing off to the release-pinned cargo-dist installer.'
  & powershell -ExecutionPolicy Bypass -File $ScriptPath @args
  exit $LASTEXITCODE
} finally {
  Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
