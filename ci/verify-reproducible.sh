#!/usr/bin/env bash
# Compare two release build directories.
#
# Usage:
#   ci/verify-reproducible.sh [--self-test] BUILD_DIR_A BUILD_DIR_B
#
# Each directory must contain exactly one
# gewu-multiverse-<commit12>-x86_64-linux.tar.gz and its .sha256 sidecar.
# The check passes only when both files are byte-identical and each sidecar
# verifies.  --self-test first proves that the same comparison rejects a copy
# with one byte changed, so a broken comparator cannot silently pass.
set -euo pipefail
umask 022

die() {
    printf 'verify-reproducible: %s\n' "$*" >&2
    exit 1
}

self_test=0
if [ "${1:-}" = "--self-test" ]; then
    self_test=1
    shift
fi
[ "$#" -eq 2 ] || {
    printf 'usage: %s [--self-test] BUILD_DIR_A BUILD_DIR_B\n' "$0" >&2
    exit 2
}

left="$(CDPATH='' cd -- "$1" && pwd -P)"
right="$(CDPATH='' cd -- "$2" && pwd -P)"

find_tarball() {
    local dir="$1" found
    found="$(find "$dir" -maxdepth 1 -type f -name '*.tar.gz' -print)"
    [ -n "$found" ] || die "$dir contains no tarball"
    [ "$(printf '%s\n' "$found" | wc -l)" -eq 1 ] ||
        die "$dir contains more than one tarball"
    printf '%s\n' "$found"
}

compare_dirs() {
    local a="$1" b="$2" ta tb
    ta="$(find_tarball "$a")"
    tb="$(find_tarball "$b")"
    [ "$(basename "$ta")" = "$(basename "$tb")" ] ||
        die "the tarball names differ: $(basename "$ta") != $(basename "$tb")"
    [ -f "$ta.sha256" ] || die "$ta.sha256 is missing"
    [ -f "$tb.sha256" ] || die "$tb.sha256 is missing"

    cmp -s "$ta" "$tb" || die "the tarballs differ"
    cmp -s "$ta.sha256" "$tb.sha256" || die "the .sha256 files differ"
    ( cd "$a" && sha256sum -c "$(basename "$ta").sha256" >/dev/null ) ||
        die "the first .sha256 does not verify"
    ( cd "$b" && sha256sum -c "$(basename "$tb").sha256" >/dev/null ) ||
        die "the second .sha256 does not verify"
}

if [ "$self_test" -eq 1 ]; then
    scratch="$(mktemp -d "${TMPDIR:-/tmp}/gewu-repro-check.XXXXXX")"
    trap 'rm -rf "$scratch"' EXIT
    right_tarball="$(find_tarball "$right")"
    cp -a "$right/." "$scratch/"
    tampered="$(find_tarball "$scratch")"
    printf '\000' | dd of="$tampered" bs=1 seek=10 conv=notrunc status=none
    cmp -s "$right_tarball" "$tampered" && die "the self-test mutation changed nothing"
    if ( compare_dirs "$left" "$scratch" ) >/dev/null 2>&1; then
        die "the self-test accepted a one-byte-tampered tarball"
    fi
    rm -rf "$scratch"
    trap - EXIT
    printf 'verify-reproducible: self-test rejected a one-byte-tampered tarball\n'
fi

compare_dirs "$left" "$right"
tarball="$(find_tarball "$left")"
sha256="$(sha256sum "$tarball" | awk '{print $1}')"
printf 'verify-reproducible: byte-identical tarballs and .sha256 files\n'
printf 'verify-reproducible: tarball=%s\n' "$(basename "$tarball")"
printf 'verify-reproducible: sha256=%s\n' "$sha256"
