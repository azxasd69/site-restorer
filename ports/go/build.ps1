# Сборка четырёх бинарников site-restorer-go.
# Запуск: pwsh -File build.ps1
$ErrorActionPreference = "Stop"

$go = "D:\dev\go\bin\go.exe"
if (-not (Test-Path $go)) { $go = "go" }

$env:GOCACHE = "D:\dev\cache\go-build"
$env:GOPATH = "D:\dev\gopath"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $root
try {
    New-Item -ItemType Directory -Force -Path dist | Out-Null

    $ldflags = "-s -w"
    $env:CGO_ENABLED = "0"

    $env:GOOS = "linux"; $env:GOARCH = "amd64"
    & $go build -trimpath -ldflags $ldflags -o dist/site-restorer-linux-x64 .

    $env:GOOS = "linux"; $env:GOARCH = "arm64"
    & $go build -trimpath -ldflags $ldflags -o dist/site-restorer-linux-arm64 .

    $env:GOOS = "windows"; $env:GOARCH = "amd64"
    & $go build -trimpath -ldflags $ldflags -o dist/site-restorer-win-x64.exe .

    $env:GOOS = "windows"; $env:GOARCH = "386"
    & $go build -trimpath -ldflags $ldflags -o dist/site-restorer-win-x86.exe .

    Get-ChildItem dist | Select-Object Name, Length | Format-Table -AutoSize
} finally {
    $env:GOOS = ""
    $env:GOARCH = ""
    $env:CGO_ENABLED = ""
    Pop-Location
}
