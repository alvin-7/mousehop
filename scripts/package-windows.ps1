param(
    [Parameter(Mandatory=$true)][string]$GtkRoot,
    [Parameter(Mandatory=$true)][string]$InnoCompiler,
    [Parameter(Mandatory=$true)][string]$VCRuntimeRoot,
    [Parameter(Mandatory=$true)][string]$RuntimeLicenseDir
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
if ((Get-Location).Path -ne $repo) { throw 'Run from the repository root' }
function Invoke-PackagingTool([string]$Command, [string[]]$Arguments, [string]$LogPath) {
    $ErrorActionPreference = 'Continue'
    & $Command @Arguments *> $LogPath
    $code = $LASTEXITCODE
    if ($code -ne 0) { throw "Tool failed ($code): $Command; see $LogPath" }
}
foreach ($item in @("$GtkRoot/bin/pkgconf.exe", "$GtkRoot/bin/gtk-4-1.dll", $InnoCompiler, $VCRuntimeRoot, $RuntimeLicenseDir)) {
    if (!(Test-Path -LiteralPath $item)) { throw "Missing packaging prerequisite: $item" }
}
$metadata = & cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cannot read Cargo package metadata' }
$version = ($metadata.packages | Where-Object name -eq 'mousehop').version
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "Unsupported installer version: $version" }
$output = Join-Path $repo "dist/windows-$version-$(Get-Date -Format yyyyMMdd-HHmmss)"
if (Test-Path -LiteralPath $output) { throw "Refusing to overwrite $output" }
$stage = Join-Path $output 'stage'
New-Item -ItemType Directory -Path "$stage/bin" | Out-Null
$env:PKG_CONFIG = "$GtkRoot/bin/pkgconf.exe"
$env:PKG_CONFIG_PATH = "$GtkRoot/lib/pkgconfig"
$env:PATH = "$GtkRoot/bin;$env:PATH"
$env:LIB = "$GtkRoot/lib;$env:LIB"
$env:INCLUDE = "$GtkRoot/include;$env:INCLUDE"
Invoke-PackagingTool 'cargo' @('build', '--release', '--locked') "$output/build.log"
Copy-Item -LiteralPath "$($metadata.target_directory)/release/mousehop.exe" -Destination "$stage/bin"
Get-ChildItem "$GtkRoot/bin/*.dll" -File | Copy-Item -Destination "$stage/bin"
Get-ChildItem "$VCRuntimeRoot/*140*.dll" -File | Copy-Item -Destination "$stage/bin"
foreach ($dir in @('lib', 'share')) { Copy-Item -LiteralPath "$GtkRoot/$dir" -Destination "$stage/$dir" -Recurse }
Copy-Item -LiteralPath $RuntimeLicenseDir -Destination "$stage/runtime-licenses" -Recurse
Copy-Item -LiteralPath "$repo/LICENSE", "$repo/NOTICE" -Destination $stage
Invoke-PackagingTool "$GtkRoot/bin/glib-compile-schemas.exe" @("$stage/share/glib-2.0/schemas") "$output/schemas.log"
Invoke-PackagingTool "$stage/bin/mousehop.exe" @('--version') "$output/launch-version.log"
Invoke-PackagingTool $InnoCompiler @("/DStageDir=$stage", "/DOutputDir=$output", "/DAppVersion=$version", "$repo/packaging/windows.iss") "$output/inno.log"
$installer = Join-Path $output "Mousehop-$version-x64-Setup.exe"
$hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $(Split-Path $installer -Leaf)" | Set-Content "$output/SHA256.txt" -Encoding UTF8
Write-Output "PACKAGE=$installer"
