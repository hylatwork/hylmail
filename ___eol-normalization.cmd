@echo off
cd /D "%~dp0"

cmd /D/C eolConverter crlf "**/.gitattributes"
cmd /D/C eolConverter crlf "**/.gitignore"
cmd /D/C eolConverter crlf "**/LICENSE*"
cmd /D/C eolConverter lf "**/*.sh"
cmd /D/C eolConverter crlf "**/*.md"
cmd /D/C eolConverter crlf "**/*.yml"
cmd /D/C eolConverter crlf "sys-src/.github/**/*.yml"
cmd /D/C eolConverter crlf "**/*.toml"
cmd /D/C eolConverter crlf "**/*.eml"
cmd /D/C eolConverter crlf "**/*.rs"
cmd /D/C eolConverter crlf "**/*.svg"
cmd /D/C eolConverter crlf "**/*.iss"
cmd /D/C eolConverter crlf "**/*.wxs"

git add --all
git status

pause