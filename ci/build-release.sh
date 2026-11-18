#!/usr/bin/env bash
# Build the x86_64 Linux release archive for gewu-multiverse reproducibly.
#
# The archive and its .sha256 sidecar are written to DIST_DIR (default: ./dist)
# and have the same layout as the 0.1.x releases:
#
#   gewu-multiverse-<commit12>-x86_64-linux/
#     LICENSE-APACHE
#     LICENSE-MIT
#     README.md
#     gewu-multiverse
#
# Build paths and timestamps are removed or normalized so that two clean
# checkouts at different paths produce the same bytes.  The script refuses to
# package a dirty worktree unless ALLOW_DIRTY=1 is set for a local experiment.
set -euo pipefail
umask 022

readonly REQUIRED_RUST="1.98.1"
readonly PRODUCT="gewu-multiverse"

die() {
    printf 'build-release: %s\n' "$*" >&2
    exit 1
}

log() {
    printf 'build-release: %s\n' "$*"
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"

absolute_path() {
    case "$1" in
        /*) realpath -m -- "$1" ;;
        *) realpath -m -- "$repo_root/$1" ;;
    esac
}

CARGO_HOME="$(absolute_path "${CARGO_HOME:-$HOME/.cargo}")"
CARGO_TARGET_DIR="$(absolute_path "${CARGO_TARGET_DIR:-$repo_root/target}")"
DIST_DIR="$(absolute_path "${DIST_DIR:-$repo_root/dist}")"

for value in "$repo_root" "$CARGO_HOME" "$CARGO_TARGET_DIR"; do
    case "$value" in
        *[[:space:]]*) die "build paths may not contain whitespace: $value" ;;
    esac
done

command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH"
command -v rustc >/dev/null 2>&1 || die "rustc is not on PATH"
command -v git >/dev/null 2>&1 || die "git is not on PATH"
command -v tar >/dev/null 2>&1 || die "tar is not on PATH"
command -v gzip >/dev/null 2>&1 || die "gzip is not on PATH"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is not on PATH"
command -v realpath >/dev/null 2>&1 || die "realpath is not on PATH"

[ "$(uname -s)" = "Linux" ] || die "the release archive is built for Linux"
[ "$(uname -m)" = "x86_64" ] || die "the release archive is built for x86_64"

rust_version="$(rustc --version | awk '{print $2}')"
[ "$rust_version" = "$REQUIRED_RUST" ] ||
    die "rustc $REQUIRED_RUST is required, found $rust_version"

git -C "$repo_root" rev-parse --verify HEAD >/dev/null 2>&1 ||
    die "$repo_root is not a git checkout"
source_rev="${SOURCE_REV:-HEAD}"
source_rev="$(git -C "$repo_root" rev-parse --verify "$source_rev")"
source_commit12="$(git -C "$repo_root" rev-parse --short=12 "$source_rev")"
[ "${#source_commit12}" -eq 12 ] || die "cannot derive a 12-character commit id"

dirty="$(git -C "$repo_root" status --porcelain --untracked-files=no)"
if [ -n "$dirty" ] && [ "${ALLOW_DIRTY:-0}" != "1" ]; then
    printf '%s\n' "$dirty" >&2
    die "the worktree has tracked changes; commit them or set ALLOW_DIRTY=1"
fi

version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo_root/Cargo.toml" | head -n 1)"
[ -n "$version" ] || die "cannot read the package version from Cargo.toml"

SOURCE_DATE_EPOCH="$(git -C "$repo_root" log -1 --format=%ct "$source_rev")"
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]] || die "cannot derive SOURCE_DATE_EPOCH"

export LC_ALL=C
export TZ=UTC
export CARGO_HOME
export CARGO_TARGET_DIR
export CARGO_INCREMENTAL=0
export ZERO_AR_DATE=1
export SOURCE_DATE_EPOCH
unset CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER

# Cargo's RUSTFLAGS is whitespace-split; the path check above makes that safe.
export RUSTFLAGS="\
--remap-path-prefix=${repo_root}=/gewu/source \
--remap-path-prefix=${CARGO_HOME}=/gewu/cargo-home \
--remap-path-prefix=${CARGO_TARGET_DIR}=/gewu/target \
-C link-arg=-Wl,--build-id=sha1"

log "building $PRODUCT $version at $source_rev"
log "source=$repo_root"
log "cargo-home=$CARGO_HOME"
log "target=$CARGO_TARGET_DIR"
log "source-date-epoch=$SOURCE_DATE_EPOCH"

cd "$repo_root"
cargo build --locked --release

binary="$CARGO_TARGET_DIR/release/$PRODUCT"
[ -x "$binary" ] || die "the release binary was not produced at $binary"

reported="$("$binary" --version)"
[ "$reported" = "$PRODUCT $version" ] ||
    die "the binary reports '$reported', expected '$PRODUCT $version'"

for needle in "$repo_root" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$HOME"; do
    [ -n "$needle" ] || continue
    if grep -a -F -q -- "$needle" "$binary"; then
        die "the binary contains a build path: $needle"
    fi
done
log "binary reports $reported and contains no build path"

archive_base="$PRODUCT-$source_commit12-x86_64-linux"
archive_name="$archive_base.tar.gz"
stage="$(mktemp -d "${TMPDIR:-/tmp}/gewu-multiverse-release.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$archive_base"

for file in LICENSE-APACHE LICENSE-MIT README.md; do
    if [ "${ALLOW_DIRTY:-0}" = "1" ]; then
        cp "$repo_root/$file" "$stage/$archive_base/$file"
    else
        git -C "$repo_root" show "$source_rev:$file" > "$stage/$archive_base/$file"
    fi
    chmod 0644 "$stage/$archive_base/$file"
done
install -m 0755 "$binary" "$stage/$archive_base/$PRODUCT"
find "$stage" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

mkdir -p "$DIST_DIR"
archive="$DIST_DIR/$archive_name"
tmp_archive="$DIST_DIR/.$archive_name.tmp.$$"
tar \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
    -C "$stage" -cf - "$archive_base" | gzip -n -9 > "$tmp_archive"
mv -f "$tmp_archive" "$archive"

expected_members="$stage/expected-members"
printf '%s\n' \
    "$archive_base/" \
    "$archive_base/LICENSE-APACHE" \
    "$archive_base/LICENSE-MIT" \
    "$archive_base/README.md" \
    "$archive_base/$PRODUCT" > "$expected_members"
tar -tzf "$archive" > "$stage/actual-members"
diff -u "$expected_members" "$stage/actual-members" >/dev/null ||
    die "the archive members do not match the release layout"

(
    cd "$DIST_DIR"
    sha256sum "$archive_name" > "$archive_name.sha256"
    sha256sum -c "$archive_name.sha256" >/dev/null
)

archive_sha="$(sha256sum "$archive" | awk '{print $1}')"
binary_sha="$(sha256sum "$binary" | awk '{print $1}')"
log "archive=$archive"
log "sha256=$archive_sha"
log "binary-sha256=$binary_sha"
printf 'ARCHIVE=%s\nSHA256=%s\nBINARY_SHA256=%s\n' \
    "$archive" "$archive_sha" "$binary_sha"
