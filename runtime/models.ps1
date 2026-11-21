<#
.SYNOPSIS
    Installs and runs findery's embedding runtime on Windows: llama.cpp behind
    llama-swap, every model on one local port.

.DESCRIPTION
    Everything lands in the runtime directory (%LOCALAPPDATA%\findery\runtime,
    or FINDERY_RUNTIME_DIR): the two pinned binaries, llama-swap.yaml, the logs,
    and a copy of this script as findery-models.ps1, so the runtime keeps
    working without the repository. The models go to the LM Studio library when
    there is one (~\.lmstudio\models), else to <runtime>\models.

    Versions and sha256 come from pins.txt; every download is checked before it
    is unpacked, and a step already done is skipped, so `install` can be run
    again at any time.

.EXAMPLE
    .\runtime\models.ps1 install            # binaries, config, models (Vulkan build)
    .\runtime\models.ps1 install -Backend cpu
    .\runtime\models.ps1 start              # in the background
    .\runtime\models.ps1 status             # up? which models are loaded?
    .\runtime\models.ps1 autostart on       # start at every logon
#>
param(
    [Parameter(Position = 0)]
    [ValidateSet('install', 'pull', 'start', 'stop', 'restart', 'status', 'ui', 'logs',
        'devices', 'autostart', 'uninstall', 'help')]
    [string] $Action = 'help',

    # `on` or `off`, for `autostart`.
    [Parameter(Position = 1)]
    [string] $Mode = '',

    # The llama.cpp build: `vulkan` runs on any recent AMD, NVIDIA or Intel GPU.
    [ValidateSet('vulkan', 'cpu')]
    [string] $Backend = '',

    # Where the GGUF files live, or go.
    [string] $Models = ''
)

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar slows a download down by an order of magnitude.
$ProgressPreference = 'SilentlyContinue'

$Here    = $PSScriptRoot
$Runtime = if ($env:FINDERY_RUNTIME_DIR) { $env:FINDERY_RUNTIME_DIR } else { Join-Path $env:LOCALAPPDATA 'findery\runtime' }
$Listen  = if ($env:FINDERY_MODELS_LISTEN) { $env:FINDERY_MODELS_LISTEN } else { '127.0.0.1:8090' }
$BaseUrl = "http://$Listen"
$Config  = Join-Path $Runtime 'llama-swap.yaml'
$State   = Join-Path $Runtime 'runtime.env'
$Log     = Join-Path $Runtime 'logs\llama-swap.log'
$RunKey  = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunName = 'findery-models'

# --- pins ---------------------------------------------------------------------

function Read-Pins {
    $pins = @{ version = @{}; asset = @{}; model = @() }
    foreach ($line in Get-Content (Join-Path $Here 'pins.txt')) {
        $line = $line.Trim()
        if ($line -eq '' -or $line.StartsWith('#')) { continue }
        $f = $line -split '\s+'
        switch ($f[0]) {
            'version' { $pins.version[$f[1]] = $f[2] }
            'asset'   { $pins.asset[$f[1]] = $f[3] }
            'model'   { $pins.model += [pscustomobject]@{ Name = $f[1]; Path = $f[2]; Sha = $f[3] } }
        }
    }
    $pins
}

$Pins     = Read-Pins
$LlamaCpp = $Pins.version['llama.cpp']
$SwapVer  = $Pins.version['llama-swap']
$Server   = Join-Path $Runtime "llama.cpp\$LlamaCpp\llama-server.exe"
$Swap     = Join-Path $Runtime "llama-swap\v$SwapVer\llama-swap.exe"

# --- state: what `install` chose, read back by every other verb ----------------

function Read-State {
    $s = @{}
    if (Test-Path $State) {
        foreach ($line in Get-Content $State) {
            if ($line -match '^\s*([A-Z_]+)=(.*)$') { $s[$Matches[1]] = $Matches[2] }
        }
    }
    $s
}

function Get-ModelsDir {
    if ($Models) { return $Models }
    $s = Read-State
    if ($s['MODELS_DIR']) { return $s['MODELS_DIR'] }
    $lmstudio = Join-Path $env:USERPROFILE '.lmstudio\models'
    if (Test-Path $lmstudio) { return $lmstudio }
    Join-Path $Runtime 'models'
}

function Get-Backend {
    if ($Backend) { return $Backend }
    $s = Read-State
    if ($s['BACKEND']) { return $s['BACKEND'] }
    'vulkan'
}

# --- downloads -----------------------------------------------------------------

function Get-Verified([string] $Url, [string] $Sha, [string] $Out) {
    if ((Test-Path $Out) -and ((Get-FileHash $Out -Algorithm SHA256).Hash -eq $Sha.ToUpper())) { return }
    New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
    $partial = "$Out.partial"
    Write-Host "  downloading $Url"
    # curl.exe ships with Windows 10 and later, shows progress, and resumes (-C -).
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($curl) {
        # curl draws its progress on stderr, which Windows PowerShell turns into
        # an error record under `Stop`: only the exit code decides here.
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & $curl.Source -L --fail --retry 3 -C - --progress-bar -o $partial $Url
        $code = $LASTEXITCODE
        $ErrorActionPreference = $previous
        if ($code -ne 0) { throw "download failed ($code): $Url" }
    } else {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $partial
    }
    $got = (Get-FileHash $partial -Algorithm SHA256).Hash
    if ($got -ne $Sha.ToUpper()) {
        Remove-Item $partial -Force
        throw "sha256 mismatch for $Url`n  expected $Sha`n  got      $($got.ToLower())"
    }
    Move-Item -Force $partial $Out
}

# Unpacks `$Zip` and moves the directory holding `$Binary` to `$Dest`.
function Expand-Into([string] $Zip, [string] $Binary, [string] $Dest) {
    $tmp = Join-Path $Runtime ".unpack-$([guid]::NewGuid().ToString('N'))"
    Expand-Archive -Path $Zip -DestinationPath $tmp -Force
    $found = Get-ChildItem -Path $tmp -Recurse -Filter $Binary | Select-Object -First 1
    if (-not $found) { Remove-Item $tmp -Recurse -Force; throw "$Binary not found in $Zip" }
    if (Test-Path $Dest) { Remove-Item $Dest -Recurse -Force }
    New-Item -ItemType Directory -Force (Split-Path $Dest) | Out-Null
    Move-Item $found.Directory.FullName $Dest
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

function Install-Binaries([string] $Backend) {
    $cache = Join-Path $Runtime 'downloads'
    if (Test-Path $Server) {
        Write-Host "llama.cpp ${LlamaCpp}: present"
    } else {
        $asset = "llama-$LlamaCpp-bin-win-$Backend-x64.zip"
        Write-Host "llama.cpp $LlamaCpp ($Backend)"
        $zip = Join-Path $cache $asset
        Get-Verified "https://github.com/ggml-org/llama.cpp/releases/download/$LlamaCpp/$asset" $Pins.asset[$asset] $zip
        Expand-Into $zip 'llama-server.exe' (Split-Path $Server)
    }
    if (Test-Path $Swap) {
        Write-Host "llama-swap v${SwapVer}: present"
    } else {
        $asset = "llama-swap_${SwapVer}_windows_amd64.zip"
        Write-Host "llama-swap v$SwapVer"
        $zip = Join-Path $cache $asset
        Get-Verified "https://github.com/mostlygeek/llama-swap/releases/download/v$SwapVer/$asset" $Pins.asset[$asset] $zip
        Expand-Into $zip 'llama-swap.exe' (Split-Path $Swap)
    }
    Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue
}

function Install-Models {
    $dir = Get-ModelsDir
    foreach ($m in $Pins.model) {
        $file = Join-Path $dir ($m.Path -replace '/', '\')
        if (Test-Path $file) { Write-Host "model $($m.Name): present"; continue }
        Write-Host "model $($m.Name)"
        $parts = $m.Path -split '/', 3
        Get-Verified "https://huggingface.co/$($parts[0])/$($parts[1])/resolve/main/$($parts[2])" $m.Sha $file
    }
}

function Write-Config([string] $Backend) {
    if (Test-Path $Config) { Write-Host "config: kept $Config"; return }
    $device = if ($Backend -eq 'cpu') { '' } else { '-ngl all' }
    # Run from the runtime copy, the template is the one `install` left beside it.
    $template = if ($Here -eq $Runtime) { Join-Path $Runtime 'templates\llama-swap.yaml' } else { Join-Path $Here 'llama-swap.yaml' }
    $text = Get-Content $template -Raw -Encoding UTF8
    $text = $text.Replace('@SERVER@', ($Server -replace '\\', '/'))
    $text = $text.Replace('@MODELS@', ((Get-ModelsDir) -replace '\\', '/'))
    $text = $text.Replace('@DEVICE@', $device)
    [IO.File]::WriteAllText($Config, $text)
    Write-Host "config: wrote $Config"
}

# --- running -------------------------------------------------------------------

function Get-SwapProcess {
    Get-Process -Name 'llama-swap' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Swap }
}

function Test-Up {
    try { Invoke-RestMethod "$BaseUrl/health" -TimeoutSec 2 | Out-Null; $true } catch { $false }
}

function Assert-Installed {
    if (-not (Test-Path $Swap) -or -not (Test-Path $Config)) {
        throw "not installed in $Runtime - run: models.ps1 install"
    }
}

function Start-Runtime {
    Assert-Installed
    if (Get-SwapProcess) { Write-Host "already running on $BaseUrl"; return }
    New-Item -ItemType Directory -Force (Split-Path $Log) | Out-Null
    $swapArgs = @('-config', "`"$Config`"", '-listen', $Listen, '-watch-config')
    Start-Process -FilePath $Swap -ArgumentList $swapArgs -WindowStyle Hidden `
        -RedirectStandardOutput $Log -RedirectStandardError "$Log.err" | Out-Null
    foreach ($i in 1..40) {
        if (Test-Up) { Write-Host "up on $BaseUrl  (dashboard: $BaseUrl/ui)"; return }
        Start-Sleep -Milliseconds 250
    }
    throw "llama-swap did not answer on $BaseUrl; see $Log.err"
}

function Stop-Runtime {
    $swap = Get-SwapProcess
    if (-not $swap) { Write-Host 'not running'; return }
    # llama-swap owns the llama-server children; unloading lets it stop them cleanly.
    try { Invoke-RestMethod -Method Post "$BaseUrl/api/models/unload" -TimeoutSec 15 | Out-Null } catch { }
    $swap | Stop-Process -Force
    Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $Server } | Stop-Process -Force
    Write-Host 'stopped'
}

function Show-Status {
    $installed = (Test-Path $Swap) -and (Test-Path $Server)
    Write-Host ("runtime   {0}{1}" -f $Runtime, $(if ($installed) { '' } else { '  (not installed)' }))
    Write-Host ("backend   {0}" -f (Get-Backend))
    Write-Host ("models    {0}" -f (Get-ModelsDir))
    $auto = (Get-ItemProperty $RunKey -Name $RunName -ErrorAction SilentlyContinue) -ne $null
    Write-Host ("autostart {0}" -f $(if ($auto) { 'on' } else { 'off' }))
    if (-not (Test-Up)) {
        Write-Host "server    not running - start it with: models.ps1 start"
    } else {
        Write-Host "server    up on $BaseUrl  (dashboard: $BaseUrl/ui)"
    }
    $loaded = @()
    if (Test-Up) { $loaded = @((Invoke-RestMethod "$BaseUrl/running" -TimeoutSec 5).running | ForEach-Object { $_.model }) }
    foreach ($m in $Pins.model) {
        $file = Join-Path (Get-ModelsDir) ($m.Path -replace '/', '\')
        $state = if (-not (Test-Path $file)) { 'missing - models.ps1 pull' } elseif ($loaded -contains $m.Name) { 'loaded' } else { 'idle' }
        Write-Host ("  {0,-38} {1}" -f $m.Name, $state)
    }
}

function Set-Autostart([string] $Turn) {
    $copy = Join-Path $Runtime 'findery-models.ps1'
    if ($Turn -eq 'on') {
        Assert-Installed
        $cmd = "powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$copy`" start"
        New-ItemProperty -Path $RunKey -Name $RunName -Value $cmd -PropertyType String -Force | Out-Null
        Write-Host "autostart on: the runtime starts at every logon"
    } elseif ($Turn -eq 'off') {
        Remove-ItemProperty -Path $RunKey -Name $RunName -ErrorAction SilentlyContinue
        Write-Host 'autostart off'
    } else {
        throw 'autostart takes on or off'
    }
}

# A copy of the tooling beside the runtime, so autostart and `findery-models.ps1`
# keep working whatever happens to the repository checkout.
function Copy-Tooling {
    if ($Here -eq $Runtime) { return }
    Copy-Item (Join-Path $Here 'models.ps1') (Join-Path $Runtime 'findery-models.ps1') -Force
    Copy-Item (Join-Path $Here 'pins.txt') (Join-Path $Runtime 'pins.txt') -Force
    New-Item -ItemType Directory -Force (Join-Path $Runtime 'templates') | Out-Null
    Copy-Item (Join-Path $Here 'llama-swap.yaml') (Join-Path $Runtime 'templates\llama-swap.yaml') -Force
}

function Show-Next {
    Write-Host ''
    Write-Host 'Point findery at it:'
    Write-Host "  findery config set url   $BaseUrl"
    Write-Host '  findery config set model harrier-oss-v1-0.6b'
    Write-Host 'The model names are the ones LM Studio uses, so vectors it computed are reused.'
}

switch ($Action) {
    'install' {
        $b = Get-Backend
        New-Item -ItemType Directory -Force $Runtime | Out-Null
        Install-Binaries $b
        Install-Models
        Write-Config $b
        [IO.File]::WriteAllText($State, "BACKEND=$b`r`nMODELS_DIR=$(Get-ModelsDir)`r`n")
        Copy-Tooling
        Write-Host "installed in $Runtime"
        Show-Next
    }
    'pull'      { Install-Models }
    'start'     { Start-Runtime }
    'stop'      { Stop-Runtime }
    'restart'   { Stop-Runtime; Start-Runtime }
    'status'    { Show-Status }
    'ui'        { if (-not (Test-Up)) { Start-Runtime }; Start-Process "$BaseUrl/ui" }
    'logs'      { Get-Content $Log -Wait -Tail 40 }
    'devices'   { Assert-Installed; & $Server --list-devices }
    'autostart' { Set-Autostart $Mode }
    'uninstall' {
        Stop-Runtime
        Set-Autostart 'off'
        foreach ($d in 'llama.cpp', 'llama-swap') { Remove-Item (Join-Path $Runtime $d) -Recurse -Force -ErrorAction SilentlyContinue }
        Write-Host "removed the binaries; kept $Config and the models in $(Get-ModelsDir)"
    }
    'help'      { Get-Help $PSCommandPath -Detailed }
}
