# Launch the shared Rune desktop app using the Windows MSVC Rust toolchain.
$ErrorActionPreference = 'Stop'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust 1.99.1 or newer with the MSVC toolchain and Visual Studio C++ build tools.'
}

$BrowserRoot = Split-Path -Parent $PSScriptRoot
Push-Location $BrowserRoot
try {
    & cargo run --locked -p browser-desktop -- @args
    $BrowserExitCode = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $BrowserExitCode
