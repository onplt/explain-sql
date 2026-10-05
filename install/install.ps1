# Installs explainsql from a GitHub release on Windows.
#
#   irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1 | iex
#   .\install.ps1 [-Version 0.1.0] [-To DIR] [-From DIR|URL]
#
# The archive's SHA-256 checksum is checked before anything is installed.
# -From installs from a directory or URL holding the release files, as the
# release workflow's smoke tests do.

param(
    [string]$Version = "",
    [string]$To = (Join-Path $env:LOCALAPPDATA "explainsql\bin"),
    [string]$From = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repo = "onplt/explain-sql"
$target = "x86_64-pc-windows-msvc"
$archive = "explainsql-$target.zip"

if ($From) {
    $base = $From
} elseif ($Version) {
    $base = "https://github.com/$repo/releases/download/v$($Version.TrimStart('v'))"
} else {
    $base = "https://github.com/$repo/releases/latest/download"
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("explainsql-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    foreach ($name in @($archive, "$archive.sha256")) {
        $destination = Join-Path $work $name
        if (Test-Path -PathType Container $base) {
            Copy-Item (Join-Path $base $name) $destination
        } else {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile $destination
        }
    }
    $expected = ((Get-Content (Join-Path $work "$archive.sha256") -Raw).Trim() -split "\s+")[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $work $archive)).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) {
        throw "checksum mismatch for ${archive}: expected $expected, got $actual"
    }
    Expand-Archive -Path (Join-Path $work $archive) -DestinationPath $work
    $binary = Join-Path $work "explainsql-$target\explainsql.exe"
    if (-not (Test-Path $binary)) {
        throw "the archive holds no explainsql.exe"
    }
    New-Item -ItemType Directory -Force -Path $To | Out-Null
    Copy-Item $binary (Join-Path $To "explainsql.exe") -Force
    $installed = & (Join-Path $To "explainsql.exe") --version
    Write-Host "Installed $installed to $(Join-Path $To 'explainsql.exe')"
    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($path -split ";") -notcontains $To) {
        Write-Host "$To is not on your PATH. To add it for your user:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$To;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
    } else {
        Write-Host "Try it: explainsql --demo"
    }
} finally {
    Remove-Item -Recurse -Force $work
}
