#!/usr/bin/env bash
# Package one release artifact into `dist/` as `<name>-<target>` plus a
# matching `.sha256`.
#
# Shared by every job in `.github/workflows/tui-release.yml` so the six
# artifacts of DESIGN.md §6.3 cannot drift apart in naming or checksum format —
# the same reasoning that produced `package-provider.sh`, which this mirrors.
#
# Usage: package-tui.sh <buzz-tui|buzz-daemon> <rust-target-triple>
set -euo pipefail

ARTIFACT="${1:?usage: package-tui.sh <buzz-tui|buzz-daemon> <rust-target-triple>}"
TARGET="${2:?usage: package-tui.sh <buzz-tui|buzz-daemon> <rust-target-triple>}"

case "$ARTIFACT" in
buzz-daemon)
  # cargo writes to target/<triple>/release/.
  SRC="target/${TARGET}/release/buzz-daemon"
  ;;
buzz-tui)
  # `tui/scripts/build.ts` names its output by the *Rust* triple deliberately,
  # so the released filenames match the daemon's and a user never has to
  # translate between Bun's target strings and cargo's.
  SRC="tui/dist/buzz-tui-${TARGET}"
  ;;
*)
  echo "::error::unknown artifact '$ARTIFACT'; expected buzz-tui or buzz-daemon" >&2
  exit 1
  ;;
esac

if [[ ! -f "$SRC" ]]; then
  echo "::error::no ${ARTIFACT} binary at ${SRC}" >&2
  exit 1
fi

NAME="${ARTIFACT}-${TARGET}"

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
