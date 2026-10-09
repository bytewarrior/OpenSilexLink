#!/bin/bash
# SPDX-License-Identifier: MIT
#
# Build the release artifacts: opensilexlink_<version>_amd64.deb and
# opensilexlink-<version>-1.x86_64.rpm.
#
#   packaging/build-packages.sh <version> [<output dir>]
#
# <version> must match the versions in daemon/Cargo.toml, client/pyproject.toml
# and client/src/opensilexlink/__init__.py. Needs cargo, python3 with pytest, and
# nfpm. Set PYTHON to use another interpreter (e.g. a venv). Used by the
# workflows; runs the same way locally.
set -euo pipefail

VERSION=${1:?usage: $0 <version> [<output dir>]}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$(realpath -m "${2:-$ROOT/dist}")
PYTHON=${PYTHON:-python3}

die() { echo "build-packages: $*" >&2; exit 1; }

# ---- versions must agree ----
cargo_ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/daemon/Cargo.toml" | head -n1)
py_ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/client/pyproject.toml")
mod_ver=$(sed -n 's/^__version__ = "\(.*\)"/\1/p' "$ROOT/client/src/opensilexlink/__init__.py")
[ "$cargo_ver" = "$VERSION" ] || die "daemon/Cargo.toml has $cargo_ver, not $VERSION"
[ "$py_ver" = "$VERSION" ] || die "client/pyproject.toml has $py_ver, not $VERSION"
[ "$mod_ver" = "$VERSION" ] || die "client/src/opensilexlink/__init__.py has $mod_ver, not $VERSION"

# ---- daemon ----
cargo test --manifest-path "$ROOT/daemon/Cargo.toml" --locked
cargo build --manifest-path "$ROOT/daemon/Cargo.toml" --release --locked

# ---- client (tests also compile the German catalog) ----
(cd "$ROOT/client" && "$PYTHON" -m pytest -q -p no:cacheprovider)
"$PYTHON" "$ROOT/client/scripts/compile_translations.py"
find "$ROOT/client/src" -name __pycache__ -type d -prune -exec rm -rf {} +

# ---- packages ----
mkdir -p "$OUT"
cd "$ROOT"
export VERSION
nfpm package -f packaging/nfpm.yaml -p deb -t "$OUT/"
nfpm package -f packaging/nfpm.yaml -p rpm -t "$OUT/"
ls -l "$OUT"
