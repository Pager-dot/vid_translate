# Windows counterpart to bench-whisper.sh.
#
# Same matrix, same one-process-per-cell rule, same reason (see bin/whisper_bench.rs).
# Two differences that matter, both Windows limitations rather than choices:
#
#  * Affinity is set with Start-Process -Affinity, which takes a bitmask rather than a CPU
#    list, and the process cannot read its own mask back without a windows-sys dependency —
#    so the binary's `affinity` column reads "n/a" and the mask is recorded in the label
#    instead. Trace a row to its mask through the label, not the column.
#  * There is no RAPL and no powerprofilesctl, so the watts and MHz columns are empty. On a
#    15W part that hurts: a power-limited cell looks identical to a computationally slow
#    one. Check the Windows power mode is "Best performance" by hand before running, and
#    treat a cell that is slow here but fast under Fedora as suspected throttling rather
#    than a real difference.
#
# Usage:
#   .\scripts\bench-whisper.ps1 -Audio eval\ja\clip01.wav -Label idle
#   .\scripts\bench-whisper.ps1 -Audio eval\ja\clip01.wav -Label browser-video -Preset full

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Audio,
  [string]$Label = "unlabelled",
  [string]$ModelsDir = "$env:LOCALAPPDATA\vid_translate\models",
  [ValidateSet("quick", "full")][string]$Preset = "quick",
  [string]$Out = "",
  [int]$WarmupMs = 0,
  [int]$MeasureMs = 0,
  [switch]$DryRun,
  [switch]$Yes
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if (-not (Test-Path $Audio)) { throw "no such audio file: $Audio" }

if ($WarmupMs -eq 0)  { $WarmupMs  = if ($Preset -eq "quick") { 20000 } else { 60000 } }
if ($MeasureMs -eq 0) { $MeasureMs = if ($Preset -eq "quick") { 20000 } else { 60000 } }
if (-not $Out) { $Out = "bench-$Label-$(Get-Date -Format 'yyyyMMdd-HHmm').csv" }

# Sorted by size, not name: alphabetically 'base' < 'small' < 'tiny', so a name sort would
# make the quick preset's "smallest and largest" pick base and tiny and skip small.
$models = @(Get-ChildItem -Path $ModelsDir -Filter "ggml-*.bin" -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -notlike "*silero*" } | Sort-Object Length)
if ($models.Count -eq 0) {
  throw "no ggml-*.bin models in $ModelsDir — download at least two sizes, or pass -ModelsDir"
}

$nproc = [Environment]::ProcessorCount

if ($Preset -eq "quick") {
  $modelSet = @($models[0].FullName)
  if ($models.Count -gt 1) { $modelSet += $models[-1].FullName }
  $threadSet = @(2, 4, [Math]::Max(2, $nproc - 2)) | Sort-Object -Unique
  $windowSet = @(10000)
} else {
  $modelSet = $models.FullName
  $threadSet = @(1, 2, 4, 6, 8, [Math]::Max(2, $nproc - 2), $nproc) | Sort-Object -Unique
  $windowSet = @(5000, 10000, 15000)
}
$flashSet = @($false, $true)

# Affinity sets as name -> bitmask. Windows gives no published P/E split, so unlike the
# Linux script this cannot be detected; these are the Alder Lake-U layout (2 P-cores with
# SMT on CPUs 0-3, 8 E-cores on CPUs 4-11) and are WRONG on any other topology. Override
# them for a different machine rather than trusting them.
$affinitySets = @(
  @{ Name = "all"; Mask = $null }
)
if ($nproc -eq 12) {
  $affinitySets += @{ Name = "pcores-0_3"; Mask = [IntPtr]0x00F }   # CPUs 0-3
  $affinitySets += @{ Name = "ecores-4_11"; Mask = [IntPtr]0xFF0 }  # CPUs 4-11
} else {
  Write-Host "note: $nproc CPUs — the hardcoded P/E masks assume a 12-thread Alder Lake-U."
  Write-Host "      Sweeping 'all' only. Edit `$affinitySets for this machine."
}

$bin = "src-tauri\target\release\whisper_bench.exe"
$cells = $affinitySets.Count * $modelSet.Count * $threadSet.Count * $windowSet.Count * $flashSet.Count
$mins = [Math]::Round($cells * ($WarmupMs + $MeasureMs) / 60000.0)

Write-Host ""
Write-Host "matrix: $cells cells, roughly $mins min of measurement (plus model load per cell)"
Write-Host "  affinity:  $($affinitySets.Name -join ' ')"
Write-Host "  models:    $(($modelSet | Split-Path -Leaf) -join ' ')"
Write-Host "  threads:   $($threadSet -join ' ')"
Write-Host "  windows:   $($windowSet -join ' ') ms"
Write-Host "  warmup:    ${WarmupMs}ms per cell, measure ${MeasureMs}ms per cell"
Write-Host "  label:     $Label"
Write-Host "  out:       $Out"

if ($DryRun) { Write-Host "`n(-DryRun: nothing executed)"; exit 0 }

if (-not (Test-Path $bin)) {
  Write-Host "`nbuilding whisper_bench (release — a debug build measures the wrong thing)"
  cargo build --release --features eval --bin whisper_bench --manifest-path src-tauri\Cargo.toml
  if ($LASTEXITCODE -ne 0) { throw "build failed" }
}

Write-Host ""
Write-Host "Check Windows power mode is 'Best performance' and the laptop is on mains."
Write-Host "Set the machine up for the '$Label' condition now (e.g. start the browser video)."
if (-not $Yes) { Read-Host "Press Enter when ready, or Ctrl-C to abort" | Out-Null }

& $bin --print-header | Set-Content -Path $Out -Encoding utf8

$cell = 0
foreach ($aff in $affinitySets) {
  foreach ($model in $modelSet) {
    foreach ($threads in $threadSet) {
      foreach ($window in $windowSet) {
        foreach ($flash in $flashSet) {
          $cell++
          $args = @(
            "--model", $model, "--audio", $Audio,
            "--threads", $threads, "--window-ms", $window,
            "--warmup-ms", $WarmupMs, "--measure-ms", $MeasureMs,
            "--label", "$Label/$($aff.Name)"
          )
          if ($flash) { $args += "--flash-attn" }

          $name = Split-Path $model -Leaf
          Write-Host "[$cell/$cells] $name threads=$threads window=${window}ms flash=$flash aff=$($aff.Name)"

          # A temp file per cell rather than appending directly: Start-Process is the only
          # way to set affinity, and it cannot append to a stream.
          $tmp = [IO.Path]::GetTempFileName()
          try {
            $sp = @{
              FilePath = $bin; ArgumentList = $args; Wait = $true; NoNewWindow = $true
              RedirectStandardOutput = $tmp
            }
            if ($aff.Mask) { $sp.Affinity = $aff.Mask }
            Start-Process @sp
            Get-Content $tmp | Where-Object { $_ -and $_ -notmatch '^label,' } |
              Add-Content -Path $Out -Encoding utf8
          } catch {
            Write-Host "  cell failed, skipped: $_"
          } finally {
            Remove-Item $tmp -ErrorAction SilentlyContinue
          }
        }
      }
    }
  }
}

Write-Host "`nwrote $Out"
Write-Host "`nCells that keep up at the minimum step (headroom_at_2s >= 1.0), best first:"
Import-Csv $Out | Sort-Object { [double]$_.headroom_at_2s } -Descending |
  Select-Object -First 20 label, model, threads, window_ms, flash_attn, mean_ms, p95_ms, headroom_at_2s |
  Format-Table -AutoSize
