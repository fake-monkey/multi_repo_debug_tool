#Requires -Version 5.1

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$BucketName = 'repo_debug'
$BucketUrl = 'https://github.com/fake-monkey/multi_repo_debug_tool.git'

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-ExecutionPolicyAllowsScoop {
    $policy = Get-ExecutionPolicy
    return $policy -in @('RemoteSigned', 'Unrestricted', 'Bypass')
}

function Confirm-ExecutionPolicyChange {
    Write-Host ''
    Write-Warning '安装和使用 Scoop 需要允许当前用户运行本地 PowerShell 脚本。'
    Write-Host '脚本准备把 CurrentUser 范围的执行策略设置为 RemoteSigned。'
    Write-Host '影响：本地脚本可以运行；从网络下载且带来源标记的脚本仍要求签名。'
    $answer = Read-Host '是否继续？请输入 Y 确认，其他输入表示取消'
    return $answer -match '^(?i:y|yes)$'
}

function Add-UserPathToCurrentProcess {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrWhiteSpace($userPath)) {
        return
    }

    $currentEntries = @($env:Path -split ';' | Where-Object { $_ })
    foreach ($entry in @($userPath -split ';' | Where-Object { $_ })) {
        if ($currentEntries -notcontains $entry) {
            $env:Path = "$env:Path;$entry"
            $currentEntries += $entry
        }
    }
}

function Install-Scoop {
    if (Test-IsAdministrator) {
        throw 'Scoop 尚未安装。请关闭管理员终端，并在普通用户 PowerShell 中重新运行本脚本。'
    }

    if (-not (Test-ExecutionPolicyAllowsScoop)) {
        if (-not (Confirm-ExecutionPolicyChange)) {
            throw '用户取消修改执行策略，未安装 Scoop。'
        }

        Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser -Force
        if (-not (Test-ExecutionPolicyAllowsScoop)) {
            throw 'CurrentUser 执行策略已设置，但有效策略仍不允许运行 Scoop；请检查组策略设置。'
        }
    }

    Write-Host ''
    Write-Host '正在通过 Scoop 官方安装脚本安装 Scoop...'
    Invoke-RestMethod -Uri 'https://get.scoop.sh' | Invoke-Expression
    Add-UserPathToCurrentProcess
}

function Get-ScoopCommand {
    $command = Get-Command scoop -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        return $null
    }
    return $command
}

function Invoke-Scoop {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$ScoopArguments
    )

    $global:LASTEXITCODE = 0
    $output = & $script:ScoopCommand @ScoopArguments
    $succeeded = $?
    $exitCode = $global:LASTEXITCODE
    if (-not $succeeded -or $exitCode -ne 0) {
        throw "Scoop 命令执行失败（退出码 $exitCode）：scoop $($ScoopArguments -join ' ')"
    }
    return $output
}

function Normalize-RepositoryUrl {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Url
    )

    $normalized = $Url.Trim().ToLowerInvariant()
    $normalized = $normalized -replace '^git@github\.com:', 'https://github.com/'
    $normalized = $normalized.TrimEnd('/')
    $normalized = $normalized -replace '\.git$', ''
    return $normalized
}

function Get-RepoDebugBucket {
    $buckets = @(Invoke-Scoop -ScoopArguments @('bucket', 'list'))
    return @(
        $buckets | Where-Object {
            $null -ne $_.PSObject.Properties['Name'] -and $_.Name -eq $BucketName
        }
    )
}

function Initialize-RepoDebugBucket {
    $matchingBuckets = @(Get-RepoDebugBucket)
    if ($matchingBuckets.Count -gt 1) {
        throw "发现多个名为 $BucketName 的 Scoop bucket，请手动清理后重试。"
    }

    if ($matchingBuckets.Count -eq 1) {
        $existingSource = [string]$matchingBuckets[0].Source
        if ((Normalize-RepositoryUrl $existingSource) -ne (Normalize-RepositoryUrl $BucketUrl)) {
            throw "Scoop bucket '$BucketName' 已存在，但指向 '$existingSource'。请手动处理，脚本不会删除或替换现有 bucket。"
        }

        Write-Host "Scoop bucket '$BucketName' 已正确添加，无需修改。"
        return
    }

    Write-Host "正在添加 Scoop bucket '$BucketName'..."
    Invoke-Scoop -ScoopArguments @('bucket', 'add', $BucketName, $BucketUrl) | Out-Host

    $addedBuckets = @(Get-RepoDebugBucket)
    if ($addedBuckets.Count -ne 1) {
        throw "执行 bucket add 后仍未找到 '$BucketName'，请检查上方 Scoop 输出。"
    }
}

function Show-RepoDebugInstructions {
    Write-Host ''
    Write-Host 'Scoop 初始化完成。'
    Write-Host ''
    Write-Host '安装（任选一个通道）：'
    Write-Host '  Stable（发布后）：'
    Write-Host '    scoop install repo_debug/repo_debug'
    Write-Host '  Alpha：'
    Write-Host '    scoop install repo_debug/repo_debug-alpha'
    Write-Host ''
    Write-Host '以后升级：'
    Write-Host '  scoop update repo_debug'
    Write-Host '  scoop update repo_debug-alpha'
}

try {
    $script:ScoopCommand = Get-ScoopCommand
    if ($null -eq $script:ScoopCommand) {
        Install-Scoop
        $script:ScoopCommand = Get-ScoopCommand
        if ($null -eq $script:ScoopCommand) {
            throw 'Scoop 安装命令已执行，但当前 PowerShell 进程仍无法找到 scoop。请重新打开终端后重试。'
        }
    }
    else {
        Write-Host '已检测到 Scoop。'
    }

    Initialize-RepoDebugBucket
    Show-RepoDebugInstructions
}
catch {
    [Console]::Error.WriteLine("ERROR: $($_.Exception.Message)")
    throw
}
