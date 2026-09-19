#!/usr/bin/env pwsh
<#
.SYNOPSIS
    The Makefile's task list, for machines without make.

.DESCRIPTION
    INTENT: `make check` is the exit checklist in CONVENTIONS.md, and Windows
    ships no make. Without this, a contributor on Windows cannot run the one
    command the project asks of them, and a user cannot install the binaries
    from the documented line.

    Every task here is the same one or three commands the Makefile runs — no
    logic lives in either file. A task added to one belongs in the other, and
    `tasks help` printing something `make help` does not is a bug.

.EXAMPLE
    .\tools\tasks.ps1 check
    .\tools\tasks.ps1 install
#>

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string] $Task = 'help'
)

$ErrorActionPreference = 'Stop'
# Every command below assumes the workspace root, exactly as make does.
Set-Location (Split-Path -Parent $PSScriptRoot)

# Runs one command and stops the whole task on failure.
#
# The exit code is the only signal read. PowerShell does not fail a script on a
# native non-zero exit, so a `check` would otherwise report success after clippy
# had rejected the build.
#
# INTENT of the preference dance: cargo writes its progress to stderr, and
# Windows PowerShell wraps a native command's stderr in an ErrorRecord whenever
# the caller redirects it — `.\tasks.ps1 check 2>&1 | ...`, or any CI that
# merges the streams. Under `Stop` that turns ordinary build output into a
# NativeCommandError and aborts a run that was going fine. Stderr is not a
# failure; the exit code is.
function Invoke-Step {
    param(
        [Parameter(Mandatory = $true)] [string]   $Exe,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )
    Write-Host "> $Exe $($Arguments -join ' ')" -ForegroundColor DarkCyan
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments
    }
    finally {
        $ErrorActionPreference = $previous
    }
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $Exe $($Arguments -join ' ')" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# Locates the bash the watchdog scripts are written for.
#
# INTENT: `bash` on the PATH of a Windows PowerShell is `System32\bash.exe`, the
# WSL launcher. It runs inside the Linux distribution, where `C:\…\findery` does
# not exist, and the watchdog fails with an execvpe error that says nothing about
# the real problem. Git for Windows is already a prerequisite, and it ships the
# right bash.
function Get-Bash {
    $candidates = @()
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($null -ne $git) {
        # ...\Git\cmd\git.exe and ...\Git\bin\git.exe both give ...\Git
        $root = Split-Path -Parent (Split-Path -Parent $git.Source)
        $candidates += (Join-Path $root 'bin\bash.exe')
        $candidates += (Join-Path $root 'usr\bin\bash.exe')
    }
    $candidates += (Join-Path $env:ProgramFiles 'Git\bin\bash.exe')
    $candidates += (Join-Path ${env:ProgramFiles(x86)} 'Git\bin\bash.exe')

    foreach ($candidate in $candidates) {
        if (-not [string]::IsNullOrEmpty($candidate) -and (Test-Path -LiteralPath $candidate)) {
            return $candidate
        }
    }
    Write-Host 'Git Bash not found; the layer and size watchdogs are shell scripts.' -ForegroundColor Red
    Write-Host 'Install Git for Windows, or run them from a Git Bash prompt.'
    exit 1
}

$tasks = [ordered]@{
    'build'        = 'Build every crate in release mode'
    'test'         = 'Run the whole test suite'
    'clippy'       = 'Lint with warnings as errors'
    'fmt'          = 'Format the workspace'
    'fmt-check'    = 'Fail if anything is unformatted'
    'layers'       = 'Verify no crate depends on a layer above it'
    'sizes'        = 'Verify no file grows past its budget (CONV-029)'
    'check'        = 'Everything CI would run'
    'bench'        = 'Measure the brute-force vector scan (release only)'
    'image'        = 'Build the daemon image'
    'up-model'     = 'Start ONLY the model; the daemon starts itself'
    'up-model-gpu' = 'Same, on the GPU'
    'run'          = 'Run the daemon in the foreground (it normally self-starts)'
    'install'      = 'Install findery, the daemon and the MCP bridge'
    'up'           = 'Start the FULL stack in Docker (daemon included)'
    'up-gpu'       = 'Start the CUDA stack'
    'up-qdrant'    = 'Start the stack with a local Qdrant vector database'
    'down'         = 'Stop everything, keep the volumes'
    'logs'         = 'Follow the daemon logs'
    'ps'           = 'Show the stack'
    'health'       = 'Ask the daemon what it is serving'
    'shell'        = 'A shell inside the daemon container'
    'clean'        = 'Remove build artefacts (the index volume is untouched)'
    'help'         = 'List the tasks'
}

switch ($Task) {
    'build'     { Invoke-Step 'cargo' @('build', '--workspace', '--release') }
    'test'      { Invoke-Step 'cargo' @('test', '--workspace') }
    'clippy'    { Invoke-Step 'cargo' @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') }
    'fmt'       { Invoke-Step 'cargo' @('fmt', '--all') }
    'fmt-check' { Invoke-Step 'cargo' @('fmt', '--all', '--', '--check') }
    'layers'    { Invoke-Step (Get-Bash) @('tools/watchdogs/check_layers.sh') }
    'sizes'     { Invoke-Step (Get-Bash) @('tools/watchdogs/check_file_size.sh') }
    'bench'     { Invoke-Step 'cargo' @('run', '-p', 'findery-bench', '--release') }
    'clean'     { Invoke-Step 'cargo' @('clean') }

    'check' {
        Invoke-Step 'cargo' @('fmt', '--all', '--', '--check')
        Invoke-Step 'cargo' @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
        Invoke-Step 'cargo' @('test', '--workspace')
        $bash = Get-Bash
        Invoke-Step $bash @('tools/watchdogs/check_layers.sh')
        Invoke-Step $bash @('tools/watchdogs/check_file_size.sh')
        Write-Host 'check OK' -ForegroundColor Green
    }

    'install' {
        Invoke-Step 'cargo' @('install', '--path', 'crates/cli', '--force')
        Invoke-Step 'cargo' @('install', '--path', 'crates/server', '--force')
        Invoke-Step 'cargo' @('install', '--path', 'crates/mcp', '--force')
        Write-Host 'findery, finderyd and findery-mcp are installed' -ForegroundColor Green
    }

    'run'          { Invoke-Step 'cargo'  @('run', '--release', '-p', 'findery-server') }
    'image'        { Invoke-Step 'docker' @('compose', 'build') }
    'up-model'     { Invoke-Step 'docker' @('compose', 'up', '-d', 'tei') }
    'up-model-gpu' { Invoke-Step 'docker' @('compose', '--profile', 'gpu', 'up', '-d', 'tei-gpu') }
    'up'           { Invoke-Step 'docker' @('compose', 'up', '-d') }
    'up-gpu'       { Invoke-Step 'docker' @('compose', '--profile', 'gpu', 'up', '-d') }
    'down'         { Invoke-Step 'docker' @('compose', '--profile', 'gpu', '--profile', 'qdrant', 'down') }
    'logs'         { Invoke-Step 'docker' @('compose', 'logs', '-f', 'findery') }
    'ps'           { Invoke-Step 'docker' @('compose', 'ps') }
    'shell'        { Invoke-Step 'docker' @('compose', 'exec', 'findery', 'bash') }

    'up-qdrant' {
        $env:FINDERY_STORE_BACKEND = 'qdrant'
        Invoke-Step 'docker' @('compose', '--profile', 'qdrant', 'up', '-d')
    }

    'health' {
        # Invoke-RestMethod rather than curl: it is present on every supported
        # PowerShell, and it prints the payload rather than a progress meter.
        $port = $env:FINDERY_PORT
        if ([string]::IsNullOrEmpty($port)) { $port = '8077' }
        Invoke-RestMethod -Uri "http://127.0.0.1:$port/v1/health" -TimeoutSec 10 | ConvertTo-Json
    }

    'help' {
        Write-Host 'findery tasks - .\tools\tasks.ps1 <task>' -ForegroundColor Cyan
        foreach ($name in $tasks.Keys) {
            Write-Host ("  {0,-13} {1}" -f $name, $tasks[$name])
        }
    }

    default {
        Write-Host "unknown task '$Task'" -ForegroundColor Red
        Write-Host 'run  .\tools\tasks.ps1 help  for the list'
        exit 1
    }
}
