#!/bin/sh
# gewu-multiverse installer — fetches the prebuilt x86_64 Linux release, verifies its
# checksum and installs the binary. No Rust toolchain is needed.
#
#   curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
#
#   sh install.sh v0.1.0            # a specific release tag instead of the latest
#   INSTALL_DIR=$HOME/bin sh install.sh
#
# Honest limits: today the releases are x86_64 Linux only, and the binary needs a
# recent glibc (2.39, Ubuntu 24.04-era). Everything else: build from source —
#   cargo install --git https://github.com/Callo42/gewu-multiverse
set -eu

repo=Callo42/gewu-multiverse
tag=${1:-}
dir=${INSTALL_DIR:-$HOME/.local/bin}
floor=2.39

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "only Linux is supported today ($(uname -s) found); build from source: cargo install --git https://github.com/$repo"
[ "$(uname -m)" = "x86_64" ] || die "only x86_64 is built today ($(uname -m) found); build from source: cargo install --git https://github.com/$repo"

for tool in curl tar sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

if [ -z "$tag" ]; then
    tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$tag" ] || die "no published release found; pass one: sh install.sh v0.1.0"
fi

json=$(curl -fsSL "https://api.github.com/repos/$repo/releases/tags/$tag") \
    || die "release $tag not found"
tarball=$(printf '%s' "$json" | grep -o 'gewu-multiverse-[A-Za-z0-9._-]*-x86_64-linux\.tar\.gz' | head -1)
[ -n "$tarball" ] || die "release $tag carries no x86_64 Linux tarball"

base="https://github.com/$repo/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# A build before 0.1.31 left no record of the version it was. Record the version the
# installed binary reports before it is replaced, so the first open of the new build
# can show the one upgrade screen with the real old version instead of guessing (W31-1).
config_dir=${XDG_CONFIG_HOME:-$HOME/.config}/gewu-multiverse
if [ -x "$dir/gewu-multiverse" ] && [ -f "$config_dir/multiverse.toml" ]; then
    old_version=$("$dir/gewu-multiverse" --version 2>/dev/null | sed -n 's/^gewu-multiverse[[:space:]]*//p' | head -1)
    if [ -n "$old_version" ]; then
        old_umask=$(umask)
        umask 077
        printf '# The newest version that has opened this configuration.\nversion = "%s"\n' "$old_version" \
            > "$config_dir/last-seen.toml" 2>/dev/null || true
        umask "$old_umask"
    fi
fi

say "downloading $tarball ($tag)"
curl -fsSL -o "$tmp/$tarball" "$base/$tarball" || die "download failed"
curl -fsSL -o "$tmp/$tarball.sha256" "$base/$tarball.sha256" || die "checksum download failed"
( cd "$tmp" && sha256sum -c "$tarball.sha256" >/dev/null 2>&1 ) || die "checksum verification failed — refusing to install"

tar -xzf "$tmp/$tarball" -C "$tmp" || die "the archive could not be unpacked"
bin=$(find "$tmp" -maxdepth 2 -type f -name gewu-multiverse | head -1)
[ -n "$bin" ] || die "the archive carries no gewu-multiverse binary"

mkdir -p "$dir"
install -m 0755 "$bin" "$dir/gewu-multiverse"
say "installed $dir/gewu-multiverse ($tag)"

on_path=0
case ":$PATH:" in
    *":$dir:"*) on_path=1 ;;
    *) say "note: $dir is not on PATH — add it with: export PATH=\"$dir:\$PATH\"" ;;
esac

if ! "$dir/gewu-multiverse" --version >/dev/null 2>&1; then
    say "note: the binary did not run here — it needs glibc >= $floor; build from source on this machine instead:"
    say "      cargo install --git https://github.com/$repo"
    exit 1
fi
# The product's own Forgejo is fetched now, once, so the first run does not have to
# wait for it. A failure is not fatal: the product is installed, and its first start
# tries again.
say "fetching Forgejo 17.0.0 for gewu-multiverse (about 122 MB; progress is shown below)"
# Keep the product's own progress on stderr: the 122 MB fetch is the longest step,
# and a silent line makes a slow connection look stuck (F7). Standard output is the
# product's ready line, which the installer replaces with its own.
if "$dir/gewu-multiverse" --install-forgejo >/dev/null; then
    say "Forgejo 17.0.0 is ready"
else
    say "the Forgejo download did not finish — the product is installed; its first start will try again"
fi

# The closing line must be a command that resolves. When the directory is not on
# PATH, the bare name would print `command not found` on the user's first command
#.
if [ "$on_path" = 1 ]; then
    say "run it with: gewu-multiverse"
else
    say "run it with: $dir/gewu-multiverse"
fi
