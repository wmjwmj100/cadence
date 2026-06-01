param(
    [string]$WecodePath = ".\target\debug\wecode.exe",
    [string]$OutDir = "",
    [string]$PromptFile = "",
    [string]$Prompt = "",
    [string]$PythonExe = "python",
    [switch]$DisableModelIo
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")

if (-not [System.IO.Path]::IsPathRooted($WecodePath)) {
    $WecodePath = Join-Path $repoRoot $WecodePath
}

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $repoRoot ("tmp\swarm-auto\" + (Get-Date -Format "yyyyMMdd_HHmmss"))
} elseif (-not [System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir = Join-Path $repoRoot $OutDir
}

if (-not (Test-Path -LiteralPath $WecodePath)) {
    throw "wecode binary not found: $WecodePath"
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

if (-not [string]::IsNullOrWhiteSpace($PromptFile)) {
    if (-not (Test-Path -LiteralPath $PromptFile)) {
        throw "prompt file not found: $PromptFile"
    }
    $Prompt = Get-Content -LiteralPath $PromptFile -Raw -Encoding UTF8
}

if ([string]::IsNullOrWhiteSpace($Prompt)) {
    $Prompt = @"
Run a high-observability swarm collaboration test:
1) Spawn 3 sub-agents in parallel.
2) Assign two sub-agents to analyze the same file `core/src/tools/handlers/call.rs`, and one sub-agent to analyze `core/src/tools/handlers/wait.rs`.
3) Use `call` to dispatch tasks and use one `wait` call to collect all replies before summarizing.
4) Include a specific, non-empty summary on every tool call.
5) End with a concise list of collaboration risks and improvement suggestions.
"@
}

$eventLogPath = Join-Path $OutDir "events.jsonl"
$stderrLogPath = Join-Path $OutDir "stderr.log"
$reportMdPath = Join-Path $OutDir "report.md"
$reportJsonPath = Join-Path $OutDir "report.json"
$modelIoDir = Join-Path $OutDir "model-io"

$originalRUSTLOG = $env:RUST_LOG
$originalDebugModelIo = $env:CODEX_DEBUG_MODEL_IO
$originalDebugModelIoDir = $env:CODEX_DEBUG_MODEL_IO_DIR
$hasNativeErrorPref = $null -ne (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue)
$originalNativeErrorPref = $null
if ($hasNativeErrorPref) {
    $originalNativeErrorPref = $PSNativeCommandUseErrorActionPreference
}

try {
    if ($hasNativeErrorPref) {
        $PSNativeCommandUseErrorActionPreference = $false
    }
    $env:RUST_LOG = "codex_core::swarm::dispatcher=debug,warn"

    if (-not $DisableModelIo) {
        $env:CODEX_DEBUG_MODEL_IO = "1"
        $env:CODEX_DEBUG_MODEL_IO_DIR = $modelIoDir
    } else {
        Remove-Item Env:CODEX_DEBUG_MODEL_IO -ErrorAction SilentlyContinue
        Remove-Item Env:CODEX_DEBUG_MODEL_IO_DIR -ErrorAction SilentlyContinue
    }

    Write-Host "Running swarm test..."
    Write-Host "  wecode: $WecodePath"
    Write-Host "  outDir: $OutDir"

    $promptPath = Join-Path $OutDir "prompt.txt"
    Set-Content -LiteralPath $promptPath -Value $Prompt -Encoding UTF8
    $swarmArgs = @(
        "exec",
        "--swarm",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
        "-"
    )
    $proc = Start-Process `
        -FilePath $WecodePath `
        -ArgumentList $swarmArgs `
        -RedirectStandardInput $promptPath `
        -RedirectStandardOutput $eventLogPath `
        -RedirectStandardError $stderrLogPath `
        -NoNewWindow `
        -PassThru `
        -Wait
    if ($proc.ExitCode -ne 0) {
        throw "wecode exited with code $($proc.ExitCode)"
    }

    Write-Host "Generating report..."
    $reportScriptPath = Join-Path $PSScriptRoot "swarm-report.py"
    $reportArgs = @(
        $reportScriptPath,
        "--event-log", $eventLogPath,
        "--stderr-log", $stderrLogPath,
        "--report", $reportMdPath,
        "--report-json", $reportJsonPath
    )
    if (-not $DisableModelIo) {
        $reportArgs += @("--model-io-dir", $modelIoDir)
    }

    & $PythonExe @reportArgs
    if ($LASTEXITCODE -ne 0) {
        throw "report generator failed with code $LASTEXITCODE"
    }

    Write-Host ""
    Write-Host "Swarm auto test complete."
    Write-Host "  event_log:  $eventLogPath"
    Write-Host "  stderr_log: $stderrLogPath"
    Write-Host "  report_md:  $reportMdPath"
    Write-Host "  report_json:$reportJsonPath"
    if (-not $DisableModelIo) {
        Write-Host "  model_io:   $modelIoDir"
    }
}
finally {
    if ($null -ne $originalRUSTLOG) {
        $env:RUST_LOG = $originalRUSTLOG
    } else {
        Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    }

    if ($null -ne $originalDebugModelIo) {
        $env:CODEX_DEBUG_MODEL_IO = $originalDebugModelIo
    } else {
        Remove-Item Env:CODEX_DEBUG_MODEL_IO -ErrorAction SilentlyContinue
    }

    if ($null -ne $originalDebugModelIoDir) {
        $env:CODEX_DEBUG_MODEL_IO_DIR = $originalDebugModelIoDir
    } else {
        Remove-Item Env:CODEX_DEBUG_MODEL_IO_DIR -ErrorAction SilentlyContinue
    }

    if ($hasNativeErrorPref) {
        $PSNativeCommandUseErrorActionPreference = $originalNativeErrorPref
    }
}
