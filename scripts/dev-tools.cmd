@echo off
rem dev-tools MCP launcher (Windows).
rem
rem All worktrees share one build output. Its location comes from
rem `git rev-parse --git-common-dir`, which opencode.jsonc cannot evaluate.
rem This launcher resolves it at startup and forwards to the real binary.
rem
rem The workspace root is not passed here: the client sets cwd to the current
rem worktree, and workspace_root() walks up from cwd, so one shared exe is
rem correctly identified per worktree.
rem
rem This file must stay ASCII-only: cmd.exe parses .cmd as OEM codepage, so
rem UTF-8 text would be garbled and break the lines.
setlocal

set "common="
for /f "usebackq delims=" %%d in (`git rev-parse --git-common-dir 2^>nul`) do set "common=%%d"
if not defined common (
	echo [dev-tools] cannot locate the git common dir ^(not in a git repo?^) 1>&2
	exit /b 1
)

set "exe=%common%\mcsup-build\debug\mcsup-dev-tools.exe"
if not exist "%exe%" (
	echo [dev-tools] missing %exe% 1>&2
	echo [dev-tools] build it once: cargo build -p dev --bin mcsup-dev-tools 1>&2
	exit /b 1
)

"%exe%" %*
exit /b %ERRORLEVEL%
