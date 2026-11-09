<#
.SYNOPSIS
    Публикация репозитория Site Restorer на GitHub.

.DESCRIPTION
    Добавляет remote (если его нет) и отправляет main + тег версии.
    Требует, чтобы репозиторий уже был создан на GitHub.

.EXAMPLE
    .\publish.ps1
    .\publish.ps1 -Https
    .\publish.ps1 -Tag v2.4
#>
param(
    [switch]$Https,
    [string]$Tag = "v2.4",
    [string]$User = "azxasd69",
    [string]$Repo = "site-restorer"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$url = if ($Https) { "https://github.com/$User/$Repo.git" } else { "git@github.com:$User/$Repo.git" }

$existing = git remote 2>$null
if ($existing -notcontains "origin") {
    Write-Host "remote origin -> $url" -ForegroundColor Cyan
    git remote add origin $url
} else {
    git remote set-url origin $url
}

Write-Host "checking repository availability..." -ForegroundColor Cyan
git ls-remote --heads origin 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Host ""
    Write-Host "Репозиторий недоступен. Сначала создайте его: https://github.com/new" -ForegroundColor Yellow
    Write-Host "  name: $Repo, public, без README/.gitignore/license" -ForegroundColor Yellow
    Write-Host "  затем повторите: .\publish.ps1" -ForegroundColor Yellow
    exit 1
}

Write-Host "pushing main..." -ForegroundColor Cyan
git push -u origin main
if ($LASTEXITCODE -ne 0) { throw "git push failed" }

if ($Tag) {
    $exists = git tag --list $Tag
    if (-not $exists) {
        git tag $Tag
    }
    Write-Host "pushing tag $Tag (запустит сборку релиза)..." -ForegroundColor Cyan
    git push origin $Tag
}

Write-Host ""
Write-Host "Готово: https://github.com/$User/$Repo" -ForegroundColor Green
Write-Host "Релиз:  https://github.com/$User/$Repo/releases/tag/$Tag" -ForegroundColor Green
