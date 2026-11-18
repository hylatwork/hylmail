@echo off

rem Removes the generated artifacts of the HylMail project:
rem   sys-src/target    - Rust build output (Cargo)
rem   sys-src/dist      - installers and archives from the packaging scripts
rem   sys-src/*.rs.bk   - rustfmt backup files
rem
rem `graphify-out/` is deliberately NOT wiped: it holds the curated community
rem labels (`.graphify_labels.json`, its `.sig` and `.pending`) and the
rem `graph.json` they attach to - knowledge, not build output. Pass `graphify`
rem to also drop the regenerable renders, reports, exports and extraction cache;
rem the labels and `graph.json` are still kept. A full reset is the deliberate
rem `graphify uninstall --purge`.
rem
rem Tracked sources, Cargo.lock and local mailbox data are left untouched.
rem
cd /D "%~dp0"

if not exist "sys-src\Cargo.toml" (
    echo ERROR: run this script from the HylMail repository root - "sys-src\Cargo.toml" not found. 1>&2
    pause
    exit /b 1
)

set "_CLEAN_GRAPH="
if /I "%~1"=="graphify"   set "_CLEAN_GRAPH=1"
if /I "%~1"=="--graphify" set "_CLEAN_GRAPH=1"

echo === HylMail - cleaning generated artifacts ==============================

rem --- Rust build output (usually by far the largest) -------------------------
if exist "sys-src\target" (
    where cargo >NUL 2>NUL
    if errorlevel 1 (
        echo [rmdir]     sys-src\target
        rmdir /S /Q "sys-src\target"
    ) else (
        echo [cargo]     clean sys-src
        cargo clean --manifest-path "sys-src\Cargo.toml"
        if exist "sys-src\target" rmdir /S /Q "sys-src\target"
    )
) else (
    echo [skip]      sys-src\target (absent^)
)

rem --- installers and archives from the packaging scripts --------------------
if exist "sys-src\dist" (
    echo [rmdir]     sys-src\dist
    rmdir /S /Q "sys-src\dist"
) else (
    echo [skip]      sys-src\dist (absent^)
)

rem --- rustfmt backup files ---------------------------------------------------
del /S /Q "sys-src\*.rs.bk" 2>NUL

rem --- knowledge graph (the curated labels are never deleted) -----------------
if exist "graphify-out" (
    if defined _CLEAN_GRAPH (
        echo [del]       graphify-out renders, reports, exports and cache
        del /Q "graphify-out\graph.html"        2>NUL
        del /Q "graphify-out\GRAPH_REPORT.md"   2>NUL
        del /Q "graphify-out\graph.svg"         2>NUL
        del /Q "graphify-out\graph.graphml"     2>NUL
        del /Q "graphify-out\cypher.txt"        2>NUL
        del /Q "graphify-out\merged-graph.json" 2>NUL
        rmdir /S /Q "graphify-out\cache"        2>NUL
        rmdir /S /Q "graphify-out\obsidian"     2>NUL
        rmdir /S /Q "graphify-out\wiki"         2>NUL
        echo [keep]      graphify-out\graph.json + .graphify_labels* ^(labels^)
    ) else (
        echo [keep]      graphify-out ^(labels^); pass "graphify" to drop the renders
    )
) else (
    echo [skip]      graphify-out (absent^)
)

echo === done ===============================================================
pause
