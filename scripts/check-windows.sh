#!/usr/bin/env bash
# Claude: copies this checkout, without target/ and .git/, to a Windows
# machine over SSH and runs the check there. The machine's SSH shell is
# PowerShell, with Rust's MSVC toolchain, the Visual Studio Build Tools, CMake
# and NASM installed. Its build directory survives between runs.
set -euo pipefail

host=${1:?usage: check-windows.sh <ssh-host> [windows-dir]}
dir=${2:-C:\\src\\spaghetti}
root=$(cd "$(dirname "$0")/.." && pwd)
archive=$(mktemp -t spaghetti-windows).tgz
trap 'rm -f "$archive"' EXIT

COPYFILE_DISABLE=1 tar czf "$archive" -C "$root" --exclude ./target --exclude ./.git .
scp -q "$archive" "$host:spaghetti-check.tgz"
ssh "$host" "\$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force '$dir' | Out-Null
Get-ChildItem -LiteralPath '$dir' -Force | Where-Object Name -ne 'target' | Remove-Item -Recurse -Force
tar -xzf \"\$env:USERPROFILE\\spaghetti-check.tgz\" -C '$dir'
Remove-Item \"\$env:USERPROFILE\\spaghetti-check.tgz\"
Set-Location '$dir'
\$env:PATH = \"C:\\Program Files\\NASM;C:\\Program Files\\CMake\\bin;\$env:USERPROFILE\\.cargo\\bin;\$env:PATH\"
\$ErrorActionPreference = 'Continue'
cargo clippy --workspace --all-targets -- -D warnings
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
cargo test --workspace
exit \$LASTEXITCODE"
