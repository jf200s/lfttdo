# T47 / spec line "cold start < 1.5s".
#
# What this measures: process start -> the main window handle exists (and, separately,
# -> the window also reports Responding). It does NOT measure "the composer can take a
# keystroke", which is the spec's own acceptance wording; that one is measured with a
# separate instrumented run and reported as a second number. See
# docs/findings/plan-c-delivery-baseline.md section 3.1 for both.
#
# THE EXE MUST COME FROM `npx tauri build`, NOT `cargo build --release`. Only the CLI
# passes `--features tauri/custom-protocol`; without it tauri-macros sets cfg(dev) and the
# app navigates to build.devUrl (http://localhost:1420) instead of its embedded assets, so
# a plain cargo-built release exe shows a chrome-error page whenever the dev server is
# down. Measuring that binary measures nothing that ships. Proof + mechanism:
# docs/findings/plan-c-delivery-baseline.md section 3.0.
#
# Comments in this file are ASCII on purpose: Windows PowerShell 5.1 reads a no-BOM
# UTF-8 .ps1 as ANSI, and a CJK byte pair can swallow the following newline, which
# merges the next statement into the comment (R233).
#
# usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File cold-start.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File cold-start.ps1 -Runs 5

param(
  [string]$Exe = (Join-Path $PSScriptRoot '..\..\..\src-tauri\target\release\qwen-do.exe'),
  [int]$Runs = 5,
  [int]$IdleMs = 1500
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path $Exe).Path

# Refuse to measure next to a running instance: the dev build is the same image name,
# and a second process tree would move the numbers (file cache, GPU process, CPU).
$leftover = @(Get-Process -Name 'qwen-do' -ErrorAction SilentlyContinue)
if ($leftover.Count -gt 0) {
  Write-Output ("ABORT: {0} qwen-do process(es) already running (PIDs {1}). Stop them first." -f `
    $leftover.Count, (($leftover | ForEach-Object { $_.Id }) -join ','))
  exit 2
}

function Measure-Once {
  param([string]$Path)
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $Path -PassThru
  $handleMs = -1
  $respondingMs = -1
  while ($sw.ElapsedMilliseconds -lt 30000) {
    try { $p.Refresh() } catch { }
    if ($handleMs -lt 0 -and $p.MainWindowHandle -ne 0) { $handleMs = $sw.ElapsedMilliseconds }
    if ($handleMs -ge 0 -and $p.Responding) { $respondingMs = $sw.ElapsedMilliseconds; break }
    Start-Sleep -Milliseconds 5
  }
  $sw.Stop()
  $pid2 = $p.Id
  Start-Sleep -Milliseconds $IdleMs
  # Kill the whole tree: the webview runs as child msedgewebview2.exe processes.
  & taskkill /PID $pid2 /T /F | Out-Null
  Start-Sleep -Milliseconds 800
  return [pscustomobject]@{ handleMs = $handleMs; respondingMs = $respondingMs }
}

Write-Output ("exe: {0}" -f $exePath)
$samples = @()
for ($i = 1; $i -le $Runs; $i++) {
  $r = Measure-Once -Path $exePath
  $samples += $r
  Write-Output ("run {0}: window-handle {1} ms, responding {2} ms" -f $i, $r.handleMs, $r.respondingMs)
}

function Median([int[]]$values) {
  $s = $values | Sort-Object
  $n = $s.Count
  if ($n -eq 0) { return -1 }
  if ($n % 2 -eq 1) { return $s[[int](($n - 1) / 2)] }
  return [math]::Round(($s[[int]($n / 2 - 1)] + $s[[int]($n / 2)]) / 2, 0)
}

$h = @($samples | ForEach-Object { $_.handleMs })
$rp = @($samples | ForEach-Object { $_.respondingMs })
Write-Output ("median window-handle: {0} ms   median responding: {1} ms   bar: 1500 ms" -f (Median $h), (Median $rp))
