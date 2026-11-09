#Requires -Version 5.1
<#
.SYNOPSIS
    The workspace gate. This is spec 9.5 step 2's "enable warnings-as-errors in CI"
    and the commands DECISIONS.md records as the test command.

.DESCRIPTION
    There is no hosted CI on this project: the repository has no remote and no
    upstream, and the reference machine is a specific laptop GPU that spec 9.12
    forbids substituting. A workflow file that never executes would claim coverage
    that does not exist, which is the same failure mode as a test in a directory
    cargo does not compile. So the gate is a script, run locally, whose exit code
    is the verdict.

    Probes are OFF by default. They are hardware-pinned and slow; the code gate runs
    on every edit, the probe gate runs when the platform facts are being re-certified.
    Passing -WithProbes re-runs them, which is required after a driver update, a
    toolchain bump, or any change to the pinned FFmpeg (spec 9.12).

.PARAMETER WithProbes
    Also run the five WP00 platform probes, including the paths that must FAIL.

.PARAMETER Only
    Run a single named step. Accepted values: fmt, clippy, test, preview-clippy,
    preview-test, probes.

.EXAMPLE
    tools\ci.cmd                      # code gate
    tools\ci.cmd -WithProbes          # full G0 revalidation
    tools\ci.cmd -Only clippy         # one step while iterating
#>
[CmdletBinding()]
param(
    [switch]$WithProbes,
    [ValidateSet('fmt', 'clippy', 'test', 'preview-clippy', 'preview-test', 'probes')]
    [string]$Only = ''
)

Set-StrictMode -Version Latest
# Native tools write to stderr routinely (rustfmt diffs, cargo progress). With
# Stop that becomes a terminating error on Windows PowerShell 7.3+, so the
# preference is set per call and the real verdict comes from $LASTEXITCODE.
$ErrorActionPreference = 'Continue'

$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $RepoRoot

# ---------------------------------------------------------------------------
# Toolchain resolution.
#
# RUSTUP_HOME and CARGO_HOME were persisted with setx, which does not reach a
# process that was already running. Every agent shell in this project therefore
# starts without cargo on PATH - observed twice during WP00. Resolving the
# binary explicitly rather than assuming PATH is what makes this script runnable
# from a fresh terminal, which is the entire point of a gate.
# ---------------------------------------------------------------------------
function Resolve-CargoPath {
    $found = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($found) { return (Split-Path -Parent $found.Source) }

    $candidates = @()
    if ($env:CARGO_HOME) { $candidates += (Join-Path $env:CARGO_HOME 'bin') }
    $candidates += 'D:\cargo\bin'
    $candidates += (Join-Path $env:USERPROFILE '.cargo\bin')

    foreach ($dir in $candidates) {
        if (Test-Path -LiteralPath (Join-Path $dir 'cargo.exe')) { return $dir }
    }
    throw 'cargo.exe not found on PATH or in CARGO_HOME, D:\cargo\bin, or the user profile. Install order and pinning are recorded in DECISIONS.md #9 and ADR-0001.'
}

$CargoBin = Resolve-CargoPath
if (-not ($env:PATH -split ';' -contains $CargoBin)) {
    $env:PATH = "$CargoBin;$env:PATH"
}

Write-Host "gate root    : $RepoRoot" -ForegroundColor DarkGray
Write-Host "cargo        : $(Join-Path $CargoBin 'cargo.exe')" -ForegroundColor DarkGray
Write-Host "cargo version: $(& cargo --version)" -ForegroundColor DarkGray

$Results = New-Object System.Collections.ArrayList

function Invoke-Step {
    <#
        Runs one named check and records the verdict. The gate must not print PASS
        because a command produced no output - only because its exit code was zero -
        which is why the code is coerced to an int before it is compared.
    #>
    param(
        [string]$Name,
        [string]$Arguments,
        [scriptblock]$Body
    )

    Write-Host ''
    Write-Host "=== $Name  [$Arguments]" -ForegroundColor Cyan
    $start = Get-Date
    $code = & $Body
    # A body that emits nothing must not be read as success by accident.
    if ($null -eq $code) { $code = 1 }
    [int]$code = $code
    $secs = [math]::Round(((Get-Date) - $start).TotalSeconds, 1)

    $passed = ($code -eq 0)
    [void]$Results.Add([pscustomobject]@{
            Step    = $Name
            Passed  = $passed
            Code    = $code
            Seconds = $secs
        })

    if ($passed) {
        Write-Host "PASS  $Name (${secs}s)" -ForegroundColor Green
    }
    else {
        Write-Host "FAIL  $Name (exit $code, ${secs}s)" -ForegroundColor Red
    }
    return $passed
}

function Invoke-Cargo {
    param([string[]]$CargoArgs)
    & cargo @CargoArgs 2>&1 | ForEach-Object { "$_" } | Write-Host
    return $LASTEXITCODE
}

# ---------------------------------------------------------------------------
# The code gates from DECISIONS.md "Test command", plus the preview-feature gates
# (DECISIONS.md #48 ruling 4). The default workspace build compiles no cpal at all;
# the preview steps run anim_audio with `preview-audio` so 17.12's device matrix and
# the live WASAPI smoke are inside the gate, not beside it.
# ---------------------------------------------------------------------------
$Wanted = if ($Only) { @($Only) } else { @('fmt', 'clippy', 'test', 'preview-clippy', 'preview-test') }
if ($WithProbes -and -not $Only) { $Wanted += 'probes' }

if ($Wanted -contains 'fmt') {
    # --check rather than a silent rewrite: a formatter that fixes the tree hides
    # the fact that the committed tree was unformatted, which is what 9.12 gates on.
    $null = Invoke-Step -Name 'fmt' -Arguments 'cargo fmt --all --check' -Body {
        Invoke-Cargo @('fmt', '--all', '--check')
    }
}

if ($Wanted -contains 'clippy') {
    # -D warnings is the spec 9.5 step 2 requirement. It only has teeth because the
    # lint set in Cargo.toml is deliberately narrow - see DECISIONS.md #11.
    $null = Invoke-Step -Name 'clippy' -Arguments 'cargo clippy --workspace --all-targets -- -D warnings' -Body {
        Invoke-Cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
    }
}

if ($Wanted -contains 'test') {
    # --locked is the load-bearing flag: without it cargo may resolve a newer
    # dependency to satisfy a missing requirement and the run passes against a
    # graph nobody validated. Spec 5.1 makes Cargo.lock authoritative after WP00.
    $null = Invoke-Step -Name 'test' -Arguments 'cargo test --workspace --locked' -Body {
        Invoke-Cargo @('test', '--workspace', '--locked')
    }
}

if ($Wanted -contains 'preview-clippy') {
    $null = Invoke-Step -Name 'preview-clippy' -Arguments 'cargo clippy -p anim_audio --all-targets --features preview-audio -- -D warnings' -Body {
        Invoke-Cargo @('clippy', '-p', 'anim_audio', '--all-targets', '--features', 'preview-audio', '--', '-D', 'warnings')
    }
}

if ($Wanted -contains 'preview-test') {
    $null = Invoke-Step -Name 'preview-test' -Arguments 'cargo test -p anim_audio --features preview-audio --locked' -Body {
        Invoke-Cargo @('test', '-p', 'anim_audio', '--features', 'preview-audio', '--locked')
    }
}

# ---------------------------------------------------------------------------
# Platform probes. Each writes probe-out/<capability>.json and returns nonzero on
# an unmet requirement (spec 9.6). Two of these scenarios are SUPPOSED to fail;
# asserting on the inverted exit code is the only way to prove those branches
# still work rather than assuming they do.
# ---------------------------------------------------------------------------
function Invoke-ProbeStep {
    <#
        Runs one probe scenario and returns the code the gate should record.

        Values arrive as parameters rather than being read off a loop variable
        inside a closure, so a scenario's expectation cannot drift onto another
        scenario - the failure mode where every probe silently inherits the last
        one's expected exit code and the inverted cases stop being inverted.
    #>
    param(
        [string]$Bin,
        [string[]]$ProbeArgs,
        [int]$Expect
    )

    $cargoRun = @('run', '-q', '-p', 'anim_test_support', '--bin', $Bin)
    if ($ProbeArgs.Count -gt 0) {
        $cargoRun += '--'
        $cargoRun += $ProbeArgs
    }
    & cargo @cargoRun 2>&1 | ForEach-Object { "$_" } | Write-Host
    $code = $LASTEXITCODE

    if ($Expect -eq 0) { return $code }
    # Invert when the scenario is meant to fail. A branch that stops failing is a
    # branch that stopped testing anything, and that is how an expected-failure case
    # quietly turns into false confidence.
    if ($code -eq 0) { return 1 }
    return 0
}

if ($Wanted -contains 'probes') {
    # name, extra args, expected exit code (0 = must pass)
    $Scenarios = @(
        @{ Label = 'wp00_gpu';                 Bin = 'wp00_gpu';     Args = @();                              Expect = 0 },
        @{ Label = 'wp00_adapter';             Bin = 'wp00_adapter'; Args = @();                              Expect = 0 },
        # IdentityUnavailable branch, spec 9.5 step 5. Must exit 1.
        @{ Label = 'wp00_adapter (unavailable)'; Bin = 'wp00_adapter'; Args = @('--force-unavailable');        Expect = 1 },
        @{ Label = 'wp00_replace (before)';    Bin = 'wp00_replace'; Args = @('--fault', 'before-commit');     Expect = 0 },
        @{ Label = 'wp00_replace (after)';     Bin = 'wp00_replace'; Args = @('--fault', 'after-commit');      Expect = 0 },
        @{ Label = 'wp00_replace (none)';      Bin = 'wp00_replace'; Args = @('--fault', 'none');              Expect = 0 },
        @{ Label = 'wp00_media';               Bin = 'wp00_media';   Args = @();                              Expect = 0 },
        @{ Label = 'wp00_export';              Bin = 'wp00_export';  Args = @();                              Expect = 0 }
    )

    foreach ($scenario in $Scenarios) {
        $argText = if ($scenario.Args.Count -gt 0) { $scenario.Args -join ' ' } else { '(default)' }
        $bin = $scenario.Bin
        $args_ = $scenario.Args
        $expect = $scenario.Expect
        $null = Invoke-Step -Name "probe $($scenario.Label)" -Arguments "$($argText) -> exit $expect" -Body {
            Invoke-ProbeStep -Bin $bin -ProbeArgs $args_ -Expect $expect
        }
    }

    Write-Host ''
    Write-Host 'NOTE: probe results are valid for THIS adapter, driver and FFmpeg build only.' -ForegroundColor Yellow
    Write-Host 'Spec 9.12 forbids carrying them to a different machine. Do not add the probes' -ForegroundColor Yellow
    Write-Host 'to a hosted runner: the reference GPU is part of the evidence.' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '================ GATE SUMMARY ================' -ForegroundColor Cyan
foreach ($r in $Results) {
    $mark = if ($r.Passed) { 'PASS' } else { 'FAIL' }
    $colour = if ($r.Passed) { 'Green' } else { 'Red' }
    Write-Host ("  {0,-34} {1,-5} {2,6}s" -f $r.Step, $mark, $r.Seconds) -ForegroundColor $colour
}

$failed = @($Results | Where-Object { -not $_.Passed })
Write-Host '==========================================' -ForegroundColor Cyan
if ($failed.Count -gt 0) {
    Write-Host ("{0} step(s) failed: {1}" -f $failed.Count, (($failed | ForEach-Object { $_.Step }) -join ', ')) -ForegroundColor Red
    Write-Host 'A failing gate outranks any summary of state, including this script''s own history.' -ForegroundColor Red
    exit 1
}

Write-Host ("all {0} step(s) passed" -f $Results.Count) -ForegroundColor Green
exit 0
