# WINapk MSIX packager — builds dist\WINapk-<ver>.msix
#
# Layout staged:  winapk.exe + winapk-display-host.exe + winapk-agent
#                 bundle\*.zip  (runtime archives — extracted by the
#                 app on first run instead of downloading; keeps the
#                 package Store-policy clean)
#                 Assets\*.png, AppxManifest.xml
#
# Usage:
#   .\make-msix.ps1                 # full build: layout + pack + dev-sign
#   .\make-msix.ps1 -NoSign         # unsigned (makeappx only)
#   .\make-msix.ps1 -SkipBundle     # omit runtime zips (slim sideload pkg)
#   .\make-msix.ps1 -Install        # trust dev cert (CurrentUser) + Add-AppxPackage
param(
    [string]$Version   = "0.1.0.0",
    [string]$Publisher = "CN=WINapk Dev",
    [switch]$NoSign,
    [switch]$SkipBundle,
    [switch]$Install
)

$ErrorActionPreference = "Stop"
$Repo    = Split-Path -Parent $PSScriptRoot
$Layout  = Join-Path $Repo "build\msix-layout"
$Dist    = Join-Path $Repo "dist"
$SdkBin  = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"
$MakeAppx = Join-Path $SdkBin "makeappx.exe"
$SignTool = Join-Path $SdkBin "signtool.exe"
$Out     = Join-Path $Dist ("WINapk-{0}.msix" -f ($Version -replace '\.0$',''))

# ---------- 0. prereqs ----------
foreach ($t in @($MakeAppx, $SignTool)) {
    if (!(Test-Path $t)) { throw "missing $t (Windows SDK)" }
}
$WinApk   = Join-Path $Repo "target\release\winapk.exe"
$DispHost = Join-Path $Repo "native\winapk-display-host\bin\winapk-display-host.exe"
$Agent    = Join-Path $Repo "target\x86_64-unknown-linux-musl\release\winapk-agent"
foreach ($f in @($WinApk, $DispHost, $Agent)) {
    if (!(Test-Path $f)) { throw "missing $f — run: cargo build --release" }
}

$Home_ = if ($env:WINAPK_HOME) { $env:WINAPK_HOME } else { Join-Path $env:LOCALAPPDATA "WINapk" }
$Runtime = Join-Path $Home_ "runtime"

# ---------- 1. layout ----------
Write-Host "[layout] $Layout"
if (Test-Path $Layout) { Remove-Item -Recurse -Force $Layout }
New-Item -ItemType Directory -Force -Path "$Layout","$Layout\Assets" | Out-Null
Copy-Item $WinApk   "$Layout\winapk.exe"
Copy-Item $DispHost "$Layout\winapk-display-host.exe"
Copy-Item $Agent    "$Layout\winapk-agent"
Copy-Item (Join-Path $Repo "packaging\AppxManifest.xml") "$Layout\AppxManifest.xml"
Copy-Item (Join-Path $Repo "packaging\assets\*.png")     "$Layout\Assets\"

# ---------- 2. bundle runtime zips ----------
New-Item -ItemType Directory -Force -Path "$Layout\bundle" | Out-Null
if (-not $SkipBundle) {
    $bundles = @(
        @{ Name = "platform-tools.zip"; Src = Join-Path $Runtime "platform-tools" },
        @{ Name = "emulator.zip";       Src = Join-Path $Runtime "emulator" },
        @{ Name = "sysimg.zip";         Src = Join-Path $Runtime "system-images\android-34\google_apis\x86_64" }
    )
    foreach ($b in $bundles) {
        $dst = "$Layout\bundle\$($b.Name)"
        $staged = Join-Path $Repo "build\bundle\$($b.Name)"
        if (Test-Path $staged) {
            # exact official zips pre-staged — just copy
            Write-Host "[bundle] staged $($b.Name)"
            Copy-Item $staged $dst
        } elseif (Test-Path $b.Src) {
            Write-Host "[bundle] repack $($b.Name) <- $($b.Src) (slow — prefer build\bundle\*.zip)"
            $parent = Split-Path -Parent $b.Src
            $leaf   = Split-Path -Leaf   $b.Src
            & tar.exe -caf $dst -C $parent $leaf | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "tar failed for $($b.Name)" }
        } else {
            Write-Warning "bundle src missing: $($b.Src) — app will download at first run"
        }
    }
}

# ---------- 3. pack ----------
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
if (Test-Path $Out) { Remove-Item $Out }
Write-Host "[pack] $Out"
& $MakeAppx pack /d $Layout /p $Out /l | Out-Null
if ($LASTEXITCODE -ne 0 -or !(Test-Path $Out)) { throw "makeappx failed" }

# ---------- 4. dev-sign ----------
if (-not $NoSign) {
    $cert = Get-ChildItem Cert:\CurrentUser\My |
            Where-Object { $_.Subject -eq $Publisher } | Select-Object -First 1
    if (-not $cert) {
        Write-Host "[cert] creating self-signed $Publisher"
        $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName "WINapk Dev" `
            -CertStoreLocation "Cert:\CurrentUser\My" `
            -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3","2.5.29.19={text}")
    }
    $pfx = Join-Path $env:TEMP "winapk-dev.pfx"
    $pwd = ConvertTo-SecureString -String "winapk-dev" -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $pwd | Out-Null
    Write-Host "[sign] $Out"
    & $SignTool sign /fd SHA256 /a /f $pfx /p "winapk-dev" $Out
    if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
    Write-Host "       cert: $($cert.Thumbprint) (self-signed — trust needed to install)"
}

# ---------- 5. optional: trust cert + install ----------
if ($Install) {
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $Publisher } | Select-Object -First 1
    if (-not $cert) { throw "no cert to trust" }
    Write-Host "[trust] importing into CurrentUser\TrustedPeople"
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store("TrustedPeople","CurrentUser")
    $store.Open("ReadWrite"); $store.Add($cert); $store.Close()
    Write-Host "[install] Add-AppxPackage $Out"
    Add-AppxPackage -Path $Out -ForceUpdateFromAnyVersion
    Get-AppxPackage -Name "Cognition.WINapk" | Select-Object Name,Version,Status
}

Write-Host ("done: {0}  ({1:N1} MB)" -f $Out, ((Get-Item $Out).Length/1MB))
