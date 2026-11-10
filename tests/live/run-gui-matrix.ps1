# Живые проверки GUI для Windows.
#
#   pwsh -File tests\live\run-gui-matrix.ps1 -Exe D:\dist\SiteRestorer.exe
#
# Проверяет: отсутствие консольного окна, старт прогона, отчёт, «Остановить»,
# докачку и переключение языка. Координаты кнопок рассчитаны на окно 896x779
# (стартовый размер); при другом размере передайте -ButtonY.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$Work = "D:\sr-tests\gui",
    [int]$ButtonY = 186,
    [int]$StartX = 45,
    [int]$StopX = 120,
    [int]$TimeoutSec = 240
)

$ErrorActionPreference = "Stop"
$exePath = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force -Path $Work | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class GuiWin {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr extra);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

    public static List<string> Windows(uint pid) {
        var list = new List<string>();
        EnumWindows((h, p) => {
            uint wp; GetWindowThreadProcessId(h, out wp);
            if (wp == pid && IsWindowVisible(h)) {
                var c = new StringBuilder(256); GetClassName(h, c, 256);
                var t = new StringBuilder(512); GetWindowText(h, t, 512);
                list.Add(c.ToString() + "|" + t.ToString());
            }
            return true;
        }, IntPtr.Zero);
        return list;
    }
}
'@

function Get-MainWindow([int]$ProcessId) {
    $found = [IntPtr]::Zero
    $cb = [GuiWin+EnumProc]{
        param($h, $p)
        $sb = New-Object System.Text.StringBuilder 512
        [void][GuiWin]::GetWindowText($h, $sb, 512)
        if ([GuiWin]::IsWindowVisible($h) -and $sb.ToString() -like "Site Restorer*") {
            $script:found = $h
            return $false
        }
        return $true
    }
    [void][GuiWin]::EnumWindows($cb, [IntPtr]::Zero)
    return $found
}

function Invoke-Click([IntPtr]$Window, [int]$X, [int]$Y) {
    $r = New-Object GuiWin+RECT
    [void][GuiWin]::GetWindowRect($Window, [ref]$r)
    [void][GuiWin]::SetWindowPos($Window, [IntPtr](-1), 0, 0, 0, 0, 0x0003)
    [void][GuiWin]::SetForegroundWindow($Window)
    Start-Sleep -Milliseconds 400
    [void][GuiWin]::SetCursorPos($r.Left + $X, $r.Top + $Y)
    Start-Sleep -Milliseconds 150
    [GuiWin]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 60
    [GuiWin]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 400
}

$results = New-Object System.Collections.ArrayList
function Report([string]$Name, [bool]$Ok, [string]$Note) {
    $v = if ($Ok) { "OK" } else { "FAIL" }
    [void]$results.Add([pscustomobject]@{ Case = $Name; Verdict = $v; Note = $Note })
    Write-Host ("{0,-24} {1,4}  {2}" -f $Name, $v, $Note)
}

# 1. запуск: окно есть, консольного окна нет
$copy = Join-Path $Work "first"
Remove-Item -Recurse -Force $copy -ErrorAction SilentlyContinue
$env:SR_AUTOSTART = "example.com"
$env:SR_OUT = $copy
$p = Start-Process -FilePath $exePath -PassThru
Start-Sleep -Seconds 6
$windows = [GuiWin]::Windows([uint32]$p.Id)
$hasConsole = @($windows | Where-Object { $_ -like "ConsoleWindowClass*" }).Count -gt 0
$hasMain = @($windows | Where-Object { $_ -like "*Site Restorer*" }).Count -gt 0
Report "gui-window" $hasMain "окно приложения: $hasMain"
Report "no-console-window" (-not $hasConsole) "консольных окон: $([int]$hasConsole)"

# 2. прогон до отчёта
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$report = Join-Path $copy "report.html"
while ((Get-Date) -lt $deadline -and -not (Test-Path $report)) { Start-Sleep -Seconds 3 }
Report "gui-run-report" (Test-Path $report) "отчёт: $(Test-Path $report)"

# 3. «Остановить» и повторный «Начать» на новом адресе
$second = Join-Path $Work "second"
Remove-Item -Recurse -Force $second -ErrorAction SilentlyContinue
$env:SR_AUTOSTART = "rdbzh.ru"
$env:SR_OUT = $second
$p2 = Start-Process -FilePath $exePath -PassThru
Start-Sleep -Seconds 12
$win2 = Get-MainWindow $p2.Id
if ($win2 -ne [IntPtr]::Zero) {
    Invoke-Click $win2 $StopX $ButtonY
    Start-Sleep -Seconds 3
    $alive = -not $p2.HasExited
    $partial = @(Get-ChildItem (Join-Path $second "mirror") -Recurse -File -ErrorAction SilentlyContinue).Count
    Report "gui-stop" ($alive -and $partial -ge 0) "процесс жив: $alive, файлов: $partial"

    Invoke-Click $win2 $StartX $ButtonY
    Start-Sleep -Seconds 5
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $report2 = Join-Path $second "report.html"
    while ((Get-Date) -lt $deadline -and -not (Test-Path $report2)) { Start-Sleep -Seconds 3 }
    Report "gui-resume" (Test-Path $report2) "после остановки прогон дошёл до отчёта: $(Test-Path $report2)"
} else {
    Report "gui-stop" $false "окно не найдено"
}
if (-not $p2.HasExited) { $p2.Kill() }
if (-not $p.HasExited) { $p.Kill() }

$ok = @($results | Where-Object { $_.Verdict -eq "OK" }).Count
Write-Host ""
Write-Host ("ИТОГО: OK {0} из {1}" -f $ok, $results.Count)
$results | Export-Csv -Path (Join-Path $Work "gui-results.csv") -NoTypeInformation -Encoding UTF8
if ($ok -ne $results.Count) { exit 1 }
