# icloud-cli installer for Windows (x64 / arm64). Non-interactive.
#   irm https://github.com/BB-fat/icloud-cli/releases/latest/download/icloud-cli-installer.ps1 | iex
# Env: ICLOUD_VERSION (e.g. v0.1.0, default latest), ICLOUD_INSTALL_DIR (default %LOCALAPPDATA%\icloud-cli\bin)
$ErrorActionPreference = 'Stop'
$Repo = 'BB-fat/icloud-cli'
$Version = if ($env:ICLOUD_VERSION) { $env:ICLOUD_VERSION } else { 'latest' }
$Dir = if ($env:ICLOUD_INSTALL_DIR) { $env:ICLOUD_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'icloud-cli\bin' }
$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
switch ($arch) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  'ARM64' { $target = 'aarch64-pc-windows-msvc' }
  default { throw "icloud-cli: unsupported architecture $arch" }
}
$asset = "icloud-cli-$target.zip"
$base = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" } else { "https://github.com/$Repo/releases/download/$Version" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile "$tmp\$asset"
  Invoke-WebRequest -UseBasicParsing "$base/$asset.sha256" -OutFile "$tmp\$asset.sha256"
  $want = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
  $got = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
  if ($want -ne $got) { throw 'icloud-cli: checksum mismatch' }
  Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Force -Path $Dir | Out-Null
  Copy-Item "$tmp\icloud-cli-$target\icloud.exe" (Join-Path $Dir 'icloud.exe') -Force
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not ($userPath -split ';' | Where-Object { $_ -eq $Dir })) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$Dir", 'User')
  }
  Write-Output (@{ ok = $true; installed = (Join-Path $Dir 'icloud.exe'); target = $target; version = $Version } | ConvertTo-Json -Compress)
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
