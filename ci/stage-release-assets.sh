#!/usr/bin/env bash
# Stage the seven files published on a gewu-multiverse release, bundling the
# verified Forgejo binary (with its GPLv3 licence and source notice as separate
# files under forgejo/) directly inside the release .tar.gz archive so a single
# unpacked archive installs and starts without a second network fetch.
#
# Usage:
#   ci/stage-release-assets.sh \
#     --build-dir BUILD_DIR \
#     --out RELEASE_ASSETS_DIR \
#     --manifest MANIFEST_DIR \
#     [--forgejo-dir FORGEJO_DIR]
#
# BUILD_DIR must contain the tarball and .sha256 produced by
# ci/build-release.sh.  FORGEJO_DIR may contain an already downloaded
# forgejo-17.0.0-linux-amd64, its .sha256 and its .asc; otherwise they are
# fetched from the official Codeberg release.  The pinned digest, the pinned
# public key and the exact bytes of FORGEJO-SOURCE.md and LICENSE are checked
# before anything is bundled or staged.  RELEASE-SHA256SUMS.txt is written
# separately from the seven release assets.
set -euo pipefail
umask 022
export LC_ALL=C
export TZ=UTC

readonly FORGEJO_NAME="forgejo-17.0.0-linux-amd64"
readonly FORGEJO_SHA256="5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca"
readonly FORGEJO_BASE_URL="${FORGEJO_BASE_URL:-https://codeberg.org/forgejo/forgejo/releases/download/v17.0.0}"
readonly SOURCE_SHA256="4f827eb1c8e0ae2c6f89282132bf7cb1855eb1a06d0f83e1f55bc90b0a3583dd"
readonly LICENSE_SHA256="3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
readonly PRIMARY_FINGERPRINT="EB114F5E6C0DC2BCDD183550A4B61A2DC5923710"
readonly SIGNING_FINGERPRINT="3BF4E813F84812411DA01E5BC4186DF66F4B6750"

die() {
    printf 'stage-release-assets: %s\n' "$*" >&2
    exit 1
}

log() {
    printf 'stage-release-assets: %s\n' "$*"
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(CDPATH='' cd -- "$script_dir/.." && pwd -P)"

for tool in gpg gzip realpath sha256sum stat tar; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not on PATH"
done

build_dir=""
forgejo_dir=""
out_dir=""
manifest_dir=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --build-dir) [ "$#" -ge 2 ] || die "--build-dir needs a value"; build_dir="$2"; shift 2 ;;
        --forgejo-dir) [ "$#" -ge 2 ] || die "--forgejo-dir needs a value"; forgejo_dir="$2"; shift 2 ;;
        --out) [ "$#" -ge 2 ] || die "--out needs a value"; out_dir="$2"; shift 2 ;;
        --manifest) [ "$#" -ge 2 ] || die "--manifest needs a value"; manifest_dir="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,22p' "$0"
            exit 0
            ;;
        *) die "unknown argument: $1" ;;
    esac
done
[ -n "$build_dir" ] || die "--build-dir is required"
[ -n "$out_dir" ] || die "--out is required"
[ -n "$manifest_dir" ] || die "--manifest is required"

build_dir="$(CDPATH='' cd -- "$build_dir" && pwd -P)"
out_dir="$(realpath -m -- "$out_dir")"
manifest_dir="$(realpath -m -- "$manifest_dir")"

find_tarball() {
    local dir="$1" found
    found="$(find "$dir" -maxdepth 1 -type f -name '*.tar.gz' -print)"
    [ -n "$found" ] || die "$dir contains no tarball"
    [ "$(printf '%s\n' "$found" | wc -l)" -eq 1 ] ||
        die "$dir contains more than one tarball"
    printf '%s\n' "$found"
}

tarball="$(find_tarball "$build_dir")"
[ -f "$tarball.sha256" ] || die "$tarball.sha256 is missing"
( cd "$build_dir" && sha256sum -c "$(basename "$tarball").sha256" >/dev/null ) ||
    die "the build checksum does not verify"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/gewu-stage-release.XXXXXX")"
cleanup() {
    rm -rf "$scratch"
}
trap cleanup EXIT

if [ -n "$forgejo_dir" ]; then
    forgejo_dir="$(CDPATH='' cd -- "$forgejo_dir" && pwd -P)"
else
    command -v curl >/dev/null 2>&1 || die "curl is not on PATH"
    forgejo_dir="$scratch/forgejo"
    mkdir -p "$forgejo_dir"
    for name in "$FORGEJO_NAME" "$FORGEJO_NAME.sha256" "$FORGEJO_NAME.asc"; do
        log "fetching $name"
        curl --fail --silent --show-error --location \
            --retry 5 --retry-all-errors \
            --output "$forgejo_dir/$name" "$FORGEJO_BASE_URL/$name" ||
            die "cannot fetch $FORGEJO_BASE_URL/$name"
    done
fi

for name in "$FORGEJO_NAME" "$FORGEJO_NAME.sha256" "$FORGEJO_NAME.asc"; do
    [ -f "$forgejo_dir/$name" ] || die "$forgejo_dir/$name is missing"
done

upstream_sha="$(tr -d '\r' < "$forgejo_dir/$FORGEJO_NAME.sha256")"
[ "$upstream_sha" = "$FORGEJO_SHA256  $FORGEJO_NAME" ] ||
    die "the upstream .sha256 does not name the pinned Forgejo digest"
printf '%s  %s\n' "$FORGEJO_SHA256" "$FORGEJO_NAME" > "$scratch/forgejo.sha256"
( cd "$forgejo_dir" && sha256sum -c "$scratch/forgejo.sha256" >/dev/null ) ||
    die "the Forgejo binary does not match the pinned digest"

gnupg_home="$scratch/gnupg"
mkdir -p "$gnupg_home"
chmod 700 "$gnupg_home"
GNUPGHOME="$gnupg_home" gpg --batch --quiet --import \
    "$repo_root/ci/release-assets/forgejo-release-signing-key.asc" ||
    die "cannot import the pinned Forgejo public key"
key_fingerprints="$(GNUPGHOME="$gnupg_home" gpg --batch --with-colons --fingerprint |
    awk -F: '$1 == "fpr" { print $10 }')"
printf '%s\n' "$key_fingerprints" | grep -qx "$PRIMARY_FINGERPRINT" ||
    die "the pinned key is not the Forgejo primary key"
printf '%s\n' "$key_fingerprints" | grep -qx "$SIGNING_FINGERPRINT" ||
    die "the pinned key does not contain the Forgejo signing subkey"
if ! GNUPGHOME="$gnupg_home" gpg --batch --verify \
    "$forgejo_dir/$FORGEJO_NAME.asc" "$forgejo_dir/$FORGEJO_NAME" \
    >"$scratch/gpg.out" 2>"$scratch/gpg.err"; then
    cat "$scratch/gpg.err" >&2
    die "the Forgejo signature does not verify"
fi
grep -q 'Good signature' "$scratch/gpg.out" "$scratch/gpg.err" ||
    die "gpg did not report a good signature"

static_dir="$repo_root/ci/release-assets"
[ "$(sha256sum "$static_dir/FORGEJO-SOURCE.md" | awk '{print $1}')" = "$SOURCE_SHA256" ] ||
    die "ci/release-assets/FORGEJO-SOURCE.md does not match the pinned SOURCE_SHA256"
[ "$(sha256sum "$static_dir/LICENSE" | awk '{print $1}')" = "$LICENSE_SHA256" ] ||
    die "ci/release-assets/LICENSE does not match the pinned LICENSE_SHA256"

mkdir -p "$out_dir" "$manifest_dir"
[ -z "$(find "$out_dir" -mindepth 1 -maxdepth 1 -print -quit)" ] ||
    die "$out_dir is not empty"
[ -z "$(find "$manifest_dir" -mindepth 1 -maxdepth 1 -print -quit)" ] ||
    die "$manifest_dir is not empty"

archive_name="$(basename "$tarball")"
archive_base="${archive_name%.tar.gz}"
[ "$archive_base" != "$archive_name" ] || die "unexpected archive name: $archive_name"

pack_bundled_archive() {
    local work="$1" dest="$2" epoch
    mkdir -p "$work/unpack"
    tar -xzf "$tarball" -C "$work/unpack"
    [ -x "$work/unpack/$archive_base/gewu-multiverse" ] ||
        die "unpacked archive lacks $archive_base/gewu-multiverse"
    epoch="$(stat -c '%Y' "$work/unpack/$archive_base/gewu-multiverse")"
    [[ "$epoch" =~ ^[0-9]+$ ]] || die "cannot read mtime epoch from unpacked binary"

    mkdir -p "$work/unpack/$archive_base/forgejo"
    chmod 0755 "$work/unpack/$archive_base/forgejo"
    install -m 0644 "$static_dir/FORGEJO-SOURCE.md" \
        "$work/unpack/$archive_base/forgejo/FORGEJO-SOURCE.md"
    install -m 0644 "$static_dir/LICENSE" \
        "$work/unpack/$archive_base/forgejo/LICENSE"
    install -m 0755 "$forgejo_dir/$FORGEJO_NAME" \
        "$work/unpack/$archive_base/forgejo/$FORGEJO_NAME"
    find "$work/unpack/$archive_base" -exec touch -h -d "@$epoch" {} +

    tar \
        --sort=name \
        --mtime="@${epoch}" \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
        -C "$work/unpack" -cf - "$archive_base" | gzip -n -9 > "$dest"
}

pack_bundled_archive "$scratch/pack-a" "$scratch/bundled-a.tar.gz"
pack_bundled_archive "$scratch/pack-b-longer-path" "$scratch/bundled-b.tar.gz"
cmp -s "$scratch/bundled-a.tar.gz" "$scratch/bundled-b.tar.gz" ||
    die "bundled release archive repack is not deterministic"

expected_members="$scratch/expected-members"
printf '%s\n' \
    "$archive_base/" \
    "$archive_base/LICENSE-APACHE" \
    "$archive_base/LICENSE-MIT" \
    "$archive_base/README.md" \
    "$archive_base/forgejo/" \
    "$archive_base/forgejo/FORGEJO-SOURCE.md" \
    "$archive_base/forgejo/LICENSE" \
    "$archive_base/forgejo/$FORGEJO_NAME" \
    "$archive_base/gewu-multiverse" > "$expected_members"
tar -tzf "$scratch/bundled-a.tar.gz" > "$scratch/actual-members"
diff -u "$expected_members" "$scratch/actual-members" >/dev/null ||
    die "bundled archive members do not match expected layout"

install -m 0644 "$scratch/bundled-a.tar.gz" "$out_dir/$archive_name"
( cd "$out_dir" && sha256sum "$archive_name" > "$archive_name.sha256" && sha256sum -c "$archive_name.sha256" >/dev/null ) ||
    die "bundled archive sidecar checksum failed"
install -m 0755 "$forgejo_dir/$FORGEJO_NAME" "$out_dir/$FORGEJO_NAME"
install -m 0644 "$forgejo_dir/$FORGEJO_NAME.sha256" "$out_dir/$FORGEJO_NAME.sha256"
install -m 0644 "$forgejo_dir/$FORGEJO_NAME.asc" "$out_dir/$FORGEJO_NAME.asc"
install -m 0644 "$static_dir/FORGEJO-SOURCE.md" "$out_dir/FORGEJO-SOURCE.md"
install -m 0644 "$static_dir/LICENSE" "$out_dir/LICENSE"

asset_count="$(find "$out_dir" -maxdepth 1 -type f | wc -l)"
[ "$asset_count" -eq 7 ] || die "expected 7 release assets, staged $asset_count"

mapfile -t asset_names < <(find "$out_dir" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
( cd "$out_dir" && sha256sum "${asset_names[@]}" ) > "$manifest_dir/RELEASE-SHA256SUMS.txt"
( cd "$out_dir" && sha256sum -c "$manifest_dir/RELEASE-SHA256SUMS.txt" >/dev/null ) ||
    die "RELEASE-SHA256SUMS.txt does not verify"

log "staged $asset_count release assets in $out_dir (bundled Forgejo inside $archive_name)"
log "wrote $manifest_dir/RELEASE-SHA256SUMS.txt"
printf 'RELEASE_ASSETS=%s\nRELEASE_MANIFEST=%s\nTARBALL_SHA256=%s\nFORGEJO_SHA256=%s\n' \
    "$out_dir" "$manifest_dir" \
    "$(sha256sum "$out_dir/$archive_name" | awk '{print $1}')" "$FORGEJO_SHA256"
