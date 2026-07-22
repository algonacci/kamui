$ErrorActionPreference = "Stop"

$Repository = "algonacci/kamui"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\kamui\bin"

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Kamui currently requires 64-bit Windows."
}

$Target = "x86_64-pc-windows-msvc"
$Archive = "kamui-$Target.zip"
$ReleaseUrl = "https://github.com/$Repository/releases/latest/download"
$TempDir = Join-Path ([IO.Path]::GetTempPath()) "kamui-install-$([Guid]::NewGuid())"

try {
    New-Item -ItemType Directory -Force -Path $TempDir, $InstallDir | Out-Null
    $ArchivePath = Join-Path $TempDir $Archive
    $ChecksumPath = "$ArchivePath.sha256"

    Invoke-WebRequest "$ReleaseUrl/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest "$ReleaseUrl/$Archive.sha256" -OutFile $ChecksumPath

    $Expected = (Get-Content $ChecksumPath -Raw).Split()[0].ToLower()
    $Actual = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToLower()
    if ($Expected -ne $Actual) {
        throw "Checksum verification failed."
    }

    Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force
    Copy-Item (Join-Path $TempDir "kamui.exe") (Join-Path $InstallDir "kamui.exe") -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ";" | Where-Object { $_ })
    if ($InstallDir -notin $PathEntries) {
        $NewPath = (($PathEntries + $InstallDir) -join ";")
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "Added $InstallDir to your user PATH."
    }

    Write-Host "Kamui installed to $InstallDir\kamui.exe"
    Write-Host "Open a new terminal and run: kamui"
}
finally {
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
