# 把一个 Windows 构建出来的 burrow.exe 打成发布产物。
#
# 用法：pwsh scripts/package-release.ps1 -Target x86_64-pc-windows-msvc
# 产物（都在 dist/）：
#   peon-burrow-<target>.zip          归档（自更新下载它）
#   burrow-<target>.exe               裸二进制（桌面端 sidecar 直接用）
#   peon-burrow-<target>.zip.sha256
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Target
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

$binary = Join-Path $root "target\$Target\release\burrow.exe"
if (-not (Test-Path $binary)) { throw "找不到构建产物：$binary" }

$archive = "peon-burrow-$Target.zip"
$archivePath = Join-Path $dist $archive
if (Test-Path $archivePath) { Remove-Item $archivePath -Force }

# 归档里放裸二进制：自更新解出来就能替换自己
Compress-Archive -Path $binary -DestinationPath $archivePath -Force
Copy-Item $binary (Join-Path $dist "burrow-$Target.exe") -Force

$hash = (Get-FileHash $archivePath -Algorithm SHA256).Hash.ToLower()
# 与 sha256sum 的输出格式保持一致，脚本两侧都能校验
Set-Content -NoNewline -Encoding ascii (Join-Path $dist "$archive.sha256") "$hash  $archive`n"

# 自更新取的是**裸二进制**（update crate 明确不解压），所以它也要有校验和
$binaryName = "burrow-$Target.exe"
Copy-Item $binary (Join-Path $dist $binaryName) -Force
$binaryHash = (Get-FileHash (Join-Path $dist $binaryName) -Algorithm SHA256).Hash.ToLower()
Set-Content -NoNewline -Encoding ascii (Join-Path $dist "$binaryName.sha256") "$binaryHash  $binaryName`n"

Write-Host '已生成：'
Get-ChildItem $dist | Select-Object Name, Length | Format-Table -AutoSize