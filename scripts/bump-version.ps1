# 统一改版本号：`[workspace.package] version` + 6 个内部依赖的版本。
#
# 用法：pwsh scripts/bump-version.ps1 -Version 0.1.0 [-DryRun]
#
# 为什么要脚本：发布时 **path 会被剥掉、只留版本**，所以内部依赖的版本必须跟着
# `[workspace.package] version` 一起改。只改一处的话，`cargo publish` 后面几个包
# 会去找一个不存在的版本 —— 而且报错信息（「no matching package named …」）
# 看起来像是网络问题，不像「你漏改了一行」。
#
# ⚠️ 只动根 `Cargo.toml` 里这 7 处。`ai-docs/` 里的 `0.1.0` 是**示例**（tag 格式、
# 输出样例），不是真版本号，脚本刻意不碰。
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    throw "不是合法的 semver：$Version（形如 0.1.0 / 0.1.0-beta.1）"
}

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$manifest = Join-Path $root 'Cargo.toml'
if (-not (Test-Path $manifest)) { throw "找不到 $manifest" }

$raw = Get-Content -Raw -LiteralPath $manifest
# 沿用它**原有的行尾与末尾状态**，别顺手规范化：
#   * `core.autocrlf = true` 时工作区是 CRLF，恒写 LF 会让 `git status` 一直显示
#     「Cargo.toml 被修改」而 `git diff` 是空的（stat 缓存不匹配），很难查；
#   * 本仓库的 Cargo.toml 末尾没有换行，硬补一个就是多一行的假 diff。
$newline = if ($raw.Contains("`r`n")) { "`r`n" } else { "`n" }
$trailingNewline = $raw.EndsWith("`n")
$lines = [System.Collections.Generic.List[string]](Get-Content -LiteralPath $manifest)
$inPackage = $false
$changed = 0
$before = @()

for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]

    if ($line -match '^\[workspace\.package\]\s*$') { $inPackage = $true; continue }
    if ($line -match '^\[') { $inPackage = $false }

    # ① workspace 版本
    if ($inPackage -and $line -match '^version = "([^"]+)"') {
        $before += "workspace.package: $($Matches[1]) → $Version"
        $lines[$i] = "version = `"$Version`""
        $changed++
        continue
    }

    # ② 内部依赖的版本（`peon-burrow-x = { version = "…", path = "…" }`）
    if ($line -match '^peon-burrow-[\w-]+ = \{ version = "([^"]+)"') {
        $before += "$($line.Split(' ')[0]): $($Matches[1]) → $Version"
        $lines[$i] = [regex]::Replace($line, '(?<=^peon-burrow-[\w-]+ = \{ version = ")[^"]+', $Version)
        $changed++
    }
}

if ($changed -ne 7) {
    throw "预期改 7 处（1 个 workspace 版本 + 6 个内部依赖），实际 $changed 处 —— 根 Cargo.toml 结构变了，先看一眼再跑"
}

$before | ForEach-Object { Write-Host "  $_" }

if ($DryRun) {
    Write-Host "  （-DryRun：没有写盘）"
    exit 0
}

# 用 LF 写回：仓库里是 LF，PowerShell 默认会写成 CRLF，那会让整个文件变成一次全量 diff
$out = ($lines -join $newline)
if ($trailingNewline) { $out += $newline }

# Windows 上文件可能被瞬时占用（编辑器、cargo、杀毒扫描都会）；这种失败重试就能过，
# 但**不能**静默吞掉 —— 一次没写成功的「改版本」比报错难查得多
$written = $false
for ($attempt = 1; $attempt -le 5; $attempt++) {
    try {
        Set-Content -LiteralPath $manifest -NoNewline -Encoding utf8 -Value $out -ErrorAction Stop
        $written = $true
        break
    } catch {
        if ($attempt -eq 5) { throw "写 $manifest 失败（被占用？）：$($_.Exception.Message)" }
        Start-Sleep -Milliseconds 300
    }
}
if (-not $written) { throw "写 $manifest 失败" }

Write-Host "  已写入 $manifest"
Write-Host "  验证（cargo metadata）："
& cargo metadata --manifest-path $manifest --no-deps --format-version 1 |
    ConvertFrom-Json |
    Select-Object -ExpandProperty packages |
    Sort-Object name |
    ForEach-Object { Write-Host ("    {0,-24} {1}" -f $_.name, $_.version) }

Write-Host '  下一步：cargo build --workspace；然后 git add Cargo.toml Cargo.lock 并提交'
