# Матрица живых тестов Site Restorer (CLI)
#
#   pwsh -File tests\live\run-cli-matrix.ps1 -Exe D:\dist\SiteRestorer-rust-win-cli-x64.exe
#   pwsh -File tests\live\run-cli-matrix.ps1 -Exe ... -Only site,fetch
#
# Группы кейсов: start (варианты запуска и путей), site (варианты адреса),
# fetch (варианты скачивания), fail (ожидаемые ошибки без паники).
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string[]]$Only = @(),
    [string]$Work = "D:\sr-tests\live",
    [int]$TimeoutSec = 600,
    [switch]$Json
)

$ErrorActionPreference = "Stop"
# pwsh -File не умеет массивы: «-Only site,fail» приходит одной строкой
$onlyList = @()
foreach ($o in $Only) { $onlyList += ($o -split ',') }
$Only = $onlyList
$results = New-Object System.Collections.ArrayList
$exePath = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force -Path $Work | Out-Null

function Run-Case {
    param(
        [string]$Name,
        [string]$Group,
        [object[]]$Arguments,
        [scriptblock]$Check,
        [scriptblock]$Prepare
    )

    if ($Only.Count -gt 0 -and -not ($Only | Where-Object { $Name -like "*$_*" -or $Group -eq $_ })) {
        return
    }
    $caseDir = Join-Path $Work $Name
    if (Test-Path $caseDir) { Remove-Item -Recurse -Force $caseDir -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force -Path $caseDir | Out-Null

    if ($Prepare) { & $Prepare @{ Dir = $caseDir } }

    $resolved = @()
    foreach ($a in $Arguments) {
        if ($null -eq $a) { continue }
        $resolved += ($a -replace '\{CASE\}', $caseDir)
    }
    $argLine = Build-ArgLine $resolved
    $log = Join-Path $caseDir "stdout.txt"
    $err = Join-Path $caseDir "stderr.txt"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    # Start-Process в PowerShell 5.1 отдаёт пустой ExitCode; работаем через .NET
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exePath
    $psi.Arguments = $argLine
    $psi.WorkingDirectory = $caseDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    # приложение пишет UTF-8; без этого PowerShell 5.1 читает как cp866
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $p = [System.Diagnostics.Process]::Start($psi)
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $finished = $p.WaitForExit($TimeoutSec * 1000)
    if (-not $finished) { try { $p.Kill() } catch { } ; $p.WaitForExit() }
    $stdout = $outTask.Result
    $stderr = $errTask.Result
    Set-Content -Path $log -Value $stdout -Encoding UTF8
    Set-Content -Path $err -Value $stderr -Encoding UTF8
    $sw.Stop()
    $code = if ($finished) { $p.ExitCode } else { -999 }

    $verdict = "FAIL"
    $note = ""
    try {
        $r = & $Check @{ Code = $code; Stdout = $stdout; Stderr = $stderr; Dir = $caseDir; Seconds = $sw.Elapsed.TotalSeconds }
        if ($r -is [string]) { $note = $r }
        if ($r -ne $false) { $verdict = "OK" }
    } catch {
        $note = $_.Exception.Message
    }

    [void]$results.Add([pscustomobject]@{
        Case    = $Name
        Group   = $Group
        Code    = $code
        Verdict = $verdict
        Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        Note    = $note
    })
    Write-Host ("{0,-28} {1,5} {2,5} {3,7}s  {4}" -f $Name, $code, $verdict, [math]::Round($sw.Elapsed.TotalSeconds, 1), $note)
}

# Start-Process не квотирует элементы массива: путь с пробелом «папка с пробелами»
# приезжал в приложение как два аргумента. Собираем строку сами.
function Build-ArgLine {
    param([object[]]$Arguments)
    $parts = @()
    foreach ($a in $Arguments) {
        if ($null -eq $a) { continue }
        $text = [string]$a
        if ($text -match '[\s"]') { $parts += '"' + ($text -replace '"', '\"') + '"' } else { $parts += $text }
    }
    return ($parts -join ' ')
}

function Report-Ok { param($D) Test-Path (Join-Path $D "report.html") }
function Site-Count { param($D) @(Get-ChildItem (Join-Path $D "site") -Recurse -File -ErrorAction SilentlyContinue).Count }
function Inv-File { param($D) Join-Path $D "data\inventory.jsonl" }
function Inv-Lines { param($D) if (Test-Path (Inv-File $D)) { @(Get-Content (Inv-File $D)).Count } else { -1 } }

$small = "example.com"
$big = "rdbzh.ru"
$koi8 = "http://www.kulichki.com"

Run-Case "start-version" "start" @("--version") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif ($c.Stdout -notmatch "site-restorer") { throw "нет названия в выводе" }
    else { "версия: $($c.Stdout.Trim())" }
}

Run-Case "start-usage" "start" @("--cli") {
    param($c)
    if ($c.Code -ne 2) { throw "ожидался код 2, получен $($c.Code)" }
    elseif ($c.Stderr -notmatch "использование") { throw "нет подсказки об использовании" }
    else { "подсказка есть" }
}

Run-Case "start-happy" "start" @("--cli", "--site", $small, "--out", "{CASE}") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code): $($c.Stderr)" }
    elseif (-not (Report-Ok $c.Dir)) { throw "нет report.html" }
    elseif ((Site-Count $c.Dir) -lt 1) { throw "нет файлов копии" }
    else { "файлов $(Site-Count $c.Dir), отчёт есть" }
}

Run-Case "start-relative-out" "start" @("--cli", "--site", $small, "--out", "copy-rel") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif (-not (Test-Path (Join-Path $c.Dir "copy-rel\site"))) { throw "относительный путь не создан" }
    else { "создан относительно рабочей папки" }
}

Run-Case "start-out-spaces-cyrillic" "start" @("--cli", "--site", $small, "--out", "папка с пробелами") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif (-not (Test-Path (Join-Path $c.Dir "папка с пробелами\report.html"))) { throw "нет отчёта" }
    else { "пробелы и кириллица в пути" }
}

Run-Case "start-out-long-path" "start" @("--cli", "--site", $small, "--out", ("длинный-путь-" + ("a" * 60) + "\" + ("b" * 60))) {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    else { "длинный путь ($(($c.Dir.Length))) создан" }
}

Run-Case "start-workers-clamp" "start" @("--cli", "--site", $small, "--limit", "1", "--workers", "999") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" } else { "потолок потоков не ломает прогон" }
}

Run-Case -Name "fail-out-is-file" -Group "fail" `
    -Arguments @("--cli", "--site", $small, "--out", "not-a-dir.txt") `
    -Prepare { param($c) Set-Content -Path (Join-Path $c.Dir "not-a-dir.txt") -Value "важные данные" } `
    -Check {
        param($c)
        $kept = Get-Content (Join-Path $c.Dir "not-a-dir.txt") -Raw -ErrorAction SilentlyContinue
        if ($c.Code -eq 0) { throw "ожидалась ошибка: путь занят файлом" }
        elseif ($c.Stdout -match "panicked") { throw "паника вместо сообщения" }
        elseif ($kept -notmatch "важные данные") { throw "существующий файл повреждён" }
        else { "ошибка без паники, файл цел" }
    }

Run-Case "fail-out-missing-drive" "fail" @("--cli", "--site", $small, "--out", "Q:\no-such\dir") {
    param($c)
    if ($c.Code -eq 0) { throw "ожидалась ошибка записи" }
    elseif ($c.Stdout -match "panicked") { throw "паника вместо ошибки" }
    else { "ошибка без паники, код $($c.Code)" }
}

Run-Case "site-with-path" "site" @("--cli", "--site", "https://rdbzh.ru/catalog/", "--out", "{CASE}", "--limit", "3") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif ((Inv-Lines $c.Dir) -le 0) { throw "пустой инвентарь" }
    else { "адресов $(Inv-Lines $c.Dir)" }
}

Run-Case "site-with-query" "site" @("--cli", "--site", "https://rdbzh.ru/?utm_source=mail&utm_campaign=x", "--out", "{CASE}", "--limit", "2") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" } else { "адресов $(Inv-Lines $c.Dir)" }
}

Run-Case "site-www" "site" @("--cli", "--site", "www.kulichki.com", "--out", "{CASE}", "--limit", "2") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" } else { "www обработан" }
}

Run-Case "site-uppercase" "site" @("--cli", "--site", "HTTPS://RDBZH.RU/", "--out", "{CASE}", "--limit", "1") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" } else { "регистр схемы и хоста" }
}

Run-Case "fail-site-garbage" "fail" @("--cli", "--site", "привет мир", "--out", "{CASE}") {
    param($c)
    if ($c.Code -eq 0) { throw "мусор принят как адрес" }
    elseif ($c.Stderr -notmatch "не разобрал адрес") { throw "непонятное сообщение" }
    else { "адрес отвергнут с подсказкой" }
}

Run-Case "fail-site-empty-scheme" "fail" @("--cli", "--site", "http://", "--out", "{CASE}") {
    param($c)
    if ($c.Code -eq 0) { throw "пустой хост принят" } else { "пустой хост отвергнут" }
}

Run-Case "fail-site-no-archive" "fail" @("--cli", "--site", "nonexistent-sub.rdbzh.ru", "--out", "{CASE}") {
    param($c)
    if ($c.Code -eq 0) { throw "ожидалась ошибка «нет страниц»" }
    elseif (($c.Stdout + $c.Stderr) -match "panicked") { throw "паника" }
    else { "ошибка без паники, код $($c.Code)" }
}

Run-Case "fail-site-not-exists" "fail" @("--cli", "--site", "no-such-domain-xyz-987654.example", "--out", "{CASE}") {
    param($c)
    if ($c.Code -eq 0) { throw "несуществующий домен дал успех" }
    elseif (($c.Stdout + $c.Stderr) -match "panicked") { throw "паника" }
    else { "ошибка без паники, код $($c.Code)" }
}

Run-Case "fetch-limit" "fetch" @("--cli", "--site", $big, "--out", "{CASE}", "--limit", "4") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif ((Inv-Lines $c.Dir) -ne 4) { throw "лимит не соблюдён: $(Inv-Lines $c.Dir)" }
    else { "ровно 4 адреса" }
}

Run-Case "fetch-rate" "fetch" @("--cli", "--site", $small, "--out", "{CASE}", "--rate", "1") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif ($c.Seconds -lt 1.0) { throw "лимит скорости не действует" }
    else { "темп ограничен, $([math]::Round($c.Seconds,1))s" }
}

Run-Case "fetch-encoding-koi8" "fetch" @("--cli", "--site", $koi8, "--out", "{CASE}", "--limit", "2") {
    param($c)
    $page = Join-Path $c.Dir "site\index.html"
    if (-not (Test-Path $page)) { throw "нет index.html" }
    elseif ((Get-Content $page -Raw -Encoding UTF8) -notmatch "АНЕКДОТЫ|МАРАЗМЫ|ЮМОР") { throw "текст не читается" }
    elseif ($c.Stdout -notmatch "кодировка|перекодировано") { throw "в журнале нет строки о кодировке" }
    else { "koi8-r распознан и перекодирован" }
}

Run-Case "fetch-mixed-404" "fetch" @("--cli", "--site", "http://old.russ.ru", "--out", "{CASE}", "--limit", "5") {
    param($c)
    if ($c.Code -ne 0) { throw "код $($c.Code)" }
    elseif ($c.Stdout -notmatch "битых") { throw "нет итоговой строки" }
    else { "прогон с отсутствующими файлами завершён" }
}

if ($results.Count -gt 0) {
    $ok = @($results | Where-Object { $_.Verdict -eq "OK" }).Count
    Write-Host ""
    Write-Host ("ИТОГО: OK {0} из {1}" -f $ok, $results.Count)
    $results | Export-Csv -Path (Join-Path $Work "results.csv") -NoTypeInformation -Encoding UTF8
    if ($Json) { $results | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Work "results.json") -Encoding UTF8 }
    if ($ok -ne $results.Count) { exit 1 }
}
