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
#   .\scripts\bench-whisper.ps1 -Audio eval\ja\clip01.wav -Label idle `
#       -PCoreMask 0x00F -ECoreMask 0xFF0        # i7-1255U: 2 P-cores + 8 E-cores

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
  [switch]$Yes,
  # Hex bitmasks, e.g. "0x00F". Opt-in because Windows publishes no hybrid-CPU split; see
  # the comment at $affinitySets.
  [string]$PCoreMask = "",
  [string]$ECoreMask = ""
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

# Affinity sets as name -> bitmask.
#
# Windows publishes no P/E split the way Linux does under /sys/devices/system/cpu/types,
# and there is no safe way to guess one: a 6-core/12-thread desktop with no E-cores at all
# has the same ProcessorCount as a 2P+8E Alder Lake-U laptop. Inferring from the count
# would label rows "pcores"/"ecores" on a CPU that has neither, which is worse than not
# sweeping affinity — a benchmark that silently mislabels its rows is the exact failure
# this harness exists to prevent.
#
# So affinity masks are opt-in and explicit. Find them with Task Manager (Performance tab,
# right-click the graph, Change graph to > Logical processors) or CoreInfo, then pass the
# bitmasks. For the i7-1255U layout (2 P-cores with SMT on CPUs 0-3, 8 E-cores on 4-11):
#
#   -PCoreMask 0x00F -ECoreMask 0xFF0
$affinitySets = @(
  @{ Name = "all"; Mask = $null }
)
if ($PCoreMask) { $affinitySets += @{ Name = "pcores-$PCoreMask"; Mask = [IntPtr][Convert]::ToInt64($PCoreMask, 16) } }
if ($ECoreMask) { $affinitySets += @{ Name = "ecores-$ECoreMask"; Mask = [IntPtr][Convert]::ToInt64($ECoreMask, 16) } }
if (-not $PCoreMask -and -not $ECoreMask) {
  Write-Host "note: sweeping 'all' CPUs only. On a hybrid CPU the P/E split is usually the"
  Write-Host "      most interesting dimension — pass -PCoreMask / -ECoreMask to sweep it."
  Write-Host "      For an i7-1255U: -PCoreMask 0x00F -ECoreMask 0xFF0"
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
  # Checked before the build rather than after: this compiles whisper.cpp and CTranslate2
  # from source, so a missing toolchain surfaces as a CMake error several minutes in, in a
  # log that does not name the real problem. The README's "Rust + Node" is not enough here.
  $missing = @()
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $missing += "cargo (Rust, MSVC toolchain: rustup default stable-msvc)" }
  if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) { $missing += "cmake (https://cmake.org/download/, or 'winget install Kitware.CMake')" }
  if (-not (Get-Command cl -ErrorAction SilentlyContinue)) {
    $missing += "cl.exe — the MSVC C++ compiler. Install Visual Studio Build Tools with the " +
                "'Desktop development with C++' workload, then run this from a " +
                "'Developer PowerShell for VS' so cl.exe is on PATH"
  }
  if ($missing.Count -gt 0) {
    Write-Host "`nCannot build — missing build prerequisites:" -ForegroundColor Yellow
    foreach ($m in $missing) { Write-Host "  - $m" }
    throw "install the above, then re-run"
  }

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
