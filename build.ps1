<#
.SYNOPSIS
    Сборка Site Restorer в один exe.

.DESCRIPTION
    Собирает портативный exe (по умолчанию x64). Запускать из корня репозитория.
    Скрипт создаёт виртуальное окружение и ставит PyInstaller указанной версии.

.EXAMPLE
    .\build.ps1
    .\build.ps1 -Arch x86
    .\build.ps1 -Both
#>
param(
    [ValidateSet("x64", "x86")] [string]$Arch = "x64",
    [switch]$Both
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

function Build-One {
    param([string]$Architecture)

    $venvName = if ($Architecture -eq "x86") { ".venv32" } else { ".venv" }
    $exeName = if ($Architecture -eq "x86") { "SiteRestorer-x86" } else { "SiteRestorer" }
    $venvPath = Join-Path $root $venvName

    if (-not (Test-Path $venvPath)) {
        $pyLauncher = if ($Architecture -eq "x86") { "py -3.14-32" } else { "py -3.14" }
        Write-Host "[$Architecture] creating $venvName ..." -ForegroundColor Cyan
        Invoke-Expression "$pyLauncher -m venv `"$venvPath`""
    }

    $pip = Join-Path $venvPath "Scripts\pip.exe"
    & $pip install --quiet --upgrade pip
    & $pip install --quiet pyinstaller==6.22.3

    $pyi = Join-Path $venvPath "Scripts\pyinstaller.exe"
    Write-Host "[$Architecture] building ..." -ForegroundColor Cyan
    & $pyi --noconfirm --onefile --windowed --name $exeName `
        --icon "$root\src\app.ico" --add-data "$root\src\app.ico;." `
        --distpath "$root\dist" --workpath "$root\build-$Architecture" `
        --specpath "$root" "$root\src\main.py"

    Write-Host "[$Architecture] done: dist\$exeName.exe" -ForegroundColor Green
}

if ($Both) {
    Build-One -Architecture "x64"
    Build-One -Architecture "x86"
} else {
    Build-One -Architecture $Arch
}
