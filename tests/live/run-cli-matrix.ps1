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
        if ($text -match '[\s"]') { $parts += '"' + $text + '"' } else { $parts += $text }
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

Run-Case "start-out-long-path" "start" @("--cli", "--site", $small, "--out", ("длинный-путь-" + ("a" * 60) + "" + ("b" * 60))) {
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

# ---------------------------------------------------------------- нагрузка
# Группа load: длинные прогоны (много файлов, обрыв, два процесса).
if ($onlyList.Count -eq 0 -or ($onlyList -contains "load")) {
    # 1. тысяча с лишним файлов: устойчивость на объёме
    $big = Join-Path $Work "load-1200"
    if (Test-Path $big) { Remove-Item -Recurse -Force $big -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force -Path $big | Out-Null
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exePath
    $psi.Arguments = "`"--cli`" `"--site`" `"anekdot.ru`" `"--out`" `"$big`" `"--limit`" `"1200`""
    $psi.WorkingDirectory = $big
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $lp = [System.Diagnostics.Process]::Start($psi)
    $outTask = $lp.StandardOutput.ReadToEndAsync()
    $errTask = $lp.StandardError.ReadToEndAsync()
    $finished = $lp.WaitForExit(3600 * 1000)
    if (-not $finished) { try { $lp.Kill() } catch { } }
    $sw.Stop()
    $lout = $outTask.Result
    $files = @(Get-ChildItem (Join-Path $big "site") -Recurse -File -ErrorAction SilentlyContinue).Count
    $hasReport = Test-Path (Join-Path $big "report.html")
    $ok = ($finished -and $lp.ExitCode -eq 0 -and $hasReport -and $files -ge 900)
    [void]$results.Add([pscustomobject]@{
        Case = "load-1200-files"; Group = "load"; Code = $lp.ExitCode
        Verdict = $(if ($ok) { "OK" } else { "FAIL" })
        Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        Note = "файлов $files, отчёт $hasReport, $([math]::Round($sw.Elapsed.TotalMinutes, 1)) мин"
    })
    Write-Host ("{0,-24} {1,5} {2,5} {3,7}s  файлов {4}, отчёт {5}" -f "load-1200-files", $lp.ExitCode, $(if ($ok) { "OK" } else { "FAIL" }), [math]::Round($sw.Elapsed.TotalSeconds, 1), $files, $hasReport)

    # 2. обрыв процесса и докачка
    $killDir = Join-Path $Work "load-kill-resume"
    if (Test-Path $killDir) { Remove-Item -Recurse -Force $killDir -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force -Path $killDir | Out-Null
    $psi2 = New-Object System.Diagnostics.ProcessStartInfo
    $psi2.FileName = $exePath
    $psi2.Arguments = "`"--cli`" `"--site`" `"anekdot.ru`" `"--out`" `"$killDir`" `"--limit`" `"200`""
    $psi2.UseShellExecute = $false
    $psi2.RedirectStandardOutput = $true
    $psi2.RedirectStandardError = $true
    $k1 = [System.Diagnostics.Process]::Start($psi2)
    Start-Sleep -Seconds 12
    $before = @(Get-ChildItem (Join-Path $killDir "mirror") -Recurse -File -ErrorAction SilentlyContinue).Count
    try { $k1.Kill() } catch { }
    Start-Sleep -Seconds 2
    $parts = @(Get-ChildItem (Join-Path $killDir "mirror") -Recurse -File -Filter "*.part" -ErrorAction SilentlyContinue).Count

    $psi3 = New-Object System.Diagnostics.ProcessStartInfo
    $psi3.FileName = $exePath
    $psi3.Arguments = "`"--cli`" `"--site`" `"anekdot.ru`" `"--out`" `"$killDir`" `"--limit`" `"200`""
    $psi3.UseShellExecute = $false
    $psi3.RedirectStandardOutput = $true
    $psi3.RedirectStandardError = $true
    $psi3.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $k2 = [System.Diagnostics.Process]::Start($psi3)
    $k2out = $k2.StandardOutput.ReadToEnd()
    $k2.WaitForExit(1800 * 1000) | Out-Null
    $after = @(Get-ChildItem (Join-Path $killDir "site") -Recurse -File -ErrorAction SilentlyContinue).Count
    $resumed = ($k2out -match "уже в папке")
    $okKill = ($k2.ExitCode -eq 0 -and $after -ge 150 -and $before -gt 0)
    [void]$results.Add([pscustomobject]@{
        Case = "load-kill-resume"; Group = "load"; Code = $k2.ExitCode
        Verdict = $(if ($okKill) { "OK" } else { "FAIL" })
        Seconds = 0
        Note = "до обрыва $before, .part $parts, после докачки $after, докачка $resumed"
    })
    Write-Host ("{0,-24} {1,5} {2,5}            до обрыва {3}, .part {4}, после {5}, докачка {6}" -f "load-kill-resume", $k2.ExitCode, $(if ($okKill) { "OK" } else { "FAIL" }), $before, $parts, $after, $resumed)

    # 3. два процесса в одну папку (конкурентность)
    $raceDir = Join-Path $Work "load-race"
    if (Test-Path $raceDir) { Remove-Item -Recurse -Force $raceDir -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Force -Path $raceDir | Out-Null
    $procs = @()
    for ($i = 0; $i -lt 2; $i++) {
        $psiR = New-Object System.Diagnostics.ProcessStartInfo
        $psiR.FileName = $exePath
        $psiR.Arguments = "`"--cli`" `"--site`" `"example.com`" `"--out`" `"$raceDir`""
        $psiR.UseShellExecute = $false
        $psiR.RedirectStandardOutput = $true
        $psiR.RedirectStandardError = $true
        $procs += [System.Diagnostics.Process]::Start($psiR)
    }
    Start-Sleep -Seconds 2
    foreach ($pp in $procs) { $pp.WaitForExit(600 * 1000) | Out-Null }
    $codes = $procs | ForEach-Object { $_.ExitCode }
    $raceReport = Test-Path (Join-Path $raceDir "report.html")
    $okRace = ($raceReport -and -not ($codes -contains -999))
    [void]$results.Add([pscustomobject]@{
        Case = "load-two-processes"; Group = "load"; Code = ($codes -join ",")
        Verdict = $(if ($okRace) { "OK" } else { "FAIL" })
        Seconds = 0
        Note = "коды $($codes -join '/'), отчёт $raceReport"
    })
    Write-Host ("{0,-24} {1,} {2,5}            коды {3}, отчёт {4}" -f "load-two-processes", "", $(if ($okRace) { "OK" } else { "FAIL" }), ($codes -join "/"), $raceReport)
}

if ($results.Count -gt 0) {
    $ok = @($results | Where-Object { $_.Verdict -eq "OK" }).Count
    Write-Host ""
    Write-Host ("ИТОГО: OK {0} из {1}" -f $ok, $results.Count)
    $results | Export-Csv -Path (Join-Path $Work "results.csv") -NoTypeInformation -Encoding UTF8
    if ($Json) { $results | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Work "results.json") -Encoding UTF8 }
    if ($ok -ne $results.Count) { exit 1 }
}
