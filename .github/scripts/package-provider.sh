#!/usr/bin/env bash
# Package a release-profile `buzz-backend-ssh` build into `dist/` as
# `buzz-backend-ssh-<target>` plus a matching `.sha256`.
#
# Shared by every job in `.github/workflows/provider-release.yml` so the four
# targets cannot drift apart in naming or checksum format. Runs under bash on
# Windows too (the workflow sets `shell: bash`), where cargo appends `.exe` to
# the built binary but the published asset keeps the target-suffixed name — the
# desktop's discovery strips `.exe` before deriving the provider id, so the
# suffix carries no meaning for an asset a user renames on install anyway.
set -euo pipefail

TARGET="${1:?usage: package-provider.sh <rust-target-triple>}"

SRC="target/${TARGET}/release/buzz-backend-ssh"
[[ -f "$SRC" ]] || SRC="${SRC}.exe"
[[ -f "$SRC" ]] || {
  echo "::error::no buzz-backend-ssh binary at target/${TARGET}/release/" >&2
  exit 1
}

NAME="buzz-backend-ssh-${TARGET}"
case "$TARGET" in
*-windows-*) NAME="${NAME}.exe" ;;
esac

mkdir -p dist
cp "$SRC" "dist/${NAME}"
chmod +x "dist/${NAME}"

# macOS runners ship `shasum`, not GNU `sha256sum`; both emit the same
# "<hex>  <name>" line that `sha256sum -c` reads back in the publish job.
if command -v sha256sum >/dev/null 2>&1; then
  (cd dist && sha256sum "${NAME}" >"${NAME}.sha256")
else
  (cd dist && shasum -a 256 "${NAME}" >"${NAME}.sha256")
fi

# Surfaced in the job log so a wrong-architecture build is visible without
# downloading the artifact.
file "dist/${NAME}" || true
cat "dist/${NAME}.sha256"
