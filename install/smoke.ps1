# Smoke test of an installed explainsql on Windows: smoke.ps1 BINARY
#
# Run by the release workflow, from the repository root.

param([Parameter(Mandatory = $true)][string]$Binary)

$ErrorActionPreference = "Stop"

function Check($name, [scriptblock]$test) {
    Write-Host -NoNewline "  $name ... "
    if (-not (& $test)) {
        Write-Host "FAILED"
        exit 1
    }
    Write-Host "ok"
}

Check "--version" { (& $Binary --version) -match '^explainsql [0-9]' }

Check "--demo --print" {
    $demo = (& $Binary --demo --print) -join "`n"
    $demo.Contains("ES001 Selective sequential scan") -and
        $demo.Contains("ES005 Expensive nested-loop inner side") -and
        $demo.Contains("CREATE INDEX CONCURRENTLY ON public.order_items (order_id);")
}

Check "a plan file, as JSON" {
    ((& $Binary --format json fixtures/pg/16/seq_scan_selective.json) -join "`n").Contains('"verdict"')
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("explainsql-smoke-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Check "a psql table on standard input, as Markdown" {
        cmd /c "`"$Binary`" --format md < fixtures\inputs\psql-aligned.txt > `"$work\report.md`""
        (Get-Content -Raw "$work\report.md").Contains("| Share | Time | Node |")
    }

    Check "--pager passes other output through" {
        [System.IO.File]::WriteAllText("$work\table.txt", " id | name`n----+------`n  1 | x`n(1 row)`n")
        cmd /c "`"$Binary`" --pager < `"$work\table.txt`" > `"$work\paged.txt`""
        (Get-FileHash "$work\table.txt").Hash -eq (Get-FileHash "$work\paged.txt").Hash
    }

    Check "input that is not a plan fails" {
        [System.IO.File]::WriteAllText("$work\hello.txt", "hello`n")
        cmd /c "`"$Binary`" < `"$work\hello.txt`" > NUL 2> NUL"
        $LASTEXITCODE -ne 0
    }
} finally {
    Remove-Item -Recurse -Force $work
}

# The last check leaves a failing exit code behind on purpose.
exit 0
