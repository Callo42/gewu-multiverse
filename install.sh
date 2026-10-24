#!/bin/sh
# gewu-multiverse installer — fetches the prebuilt x86_64 Linux release, verifies its
# checksum and installs the binary. No Rust toolchain is needed.
#
#   curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
#
#   sh install.sh v0.1.0            # a specific release tag instead of the latest
#   sh install.sh --harness pi,codex
#   GEWU_HARNESSES=pi,opencode sh install.sh
#   INSTALL_DIR=$HOME/bin sh install.sh
#
# The default program is Pi. --harness none installs only the built-in engine.
# Ctrl-C during a download, unpack or program install cancels it; the binary that
# was already installed stays in place.
#
# Honest limits: today the releases are x86_64 Linux only, and the binary needs a
# recent glibc (2.39, Ubuntu 24.04-era). Everything else: build from source —
#   cargo install --git https://github.com/Callo42/gewu-multiverse
set -eu

repo=Callo42/gewu-multiverse
tag=
harnesses=${GEWU_HARNESSES:-pi}
dir=${INSTALL_DIR:-$HOME/.local/bin}
floor=2.39
tmp=
lock=
newbin=
child=
say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --harness)
            [ "$#" -ge 2 ] || die "--harness needs a comma-separated list"
            harnesses=$2
            shift 2
            ;;
        --harness=*)
            harnesses=${1#--harness=}
            shift
            ;;
        --)
            shift
            [ "$#" -ge 1 ] && tag=$1
            break
            ;;
        --*)
            die "unknown option $1"
            ;;
        *)
            tag=$1
            shift
            ;;
    esac
done

# Reject an unknown choice before downloading anything. The words are the same
# names the product accepts on its Program screen.
old_ifs=$IFS
IFS=,
for program in $harnesses; do
    case "$program" in
        ""|none|pi|codex|dsh|opencode) ;;
        *) IFS=$old_ifs; die "unknown harness '$program' (choose pi, codex, dsh, opencode or none)" ;;
    esac
done
IFS=$old_ifs

[ "$(uname -s)" = "Linux" ] || die "only Linux is supported today ($(uname -s) found); build from source: cargo install --git https://github.com/$repo"
[ "$(uname -m)" = "x86_64" ] || die "only x86_64 is built today ($(uname -m) found); build from source: cargo install --git https://github.com/$repo"

for tool in curl tar sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

release_lock() {
    if [ -n "$lock" ] && [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$$" ]; then
        rm -f "$lock/pid" "$lock/token" 2>/dev/null || true
        rmdir "$lock" 2>/dev/null || true
        lock=
    fi
}

cleanup() {
    if [ -n "$child" ]; then
        kill "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
        child=
    fi
    if [ -n "$newbin" ]; then
        rm -f "$newbin" 2>/dev/null || true
        newbin=
    fi
    if [ -n "$tmp" ]; then
        rm -rf "$tmp" 2>/dev/null || true
        tmp=
    fi
    release_lock
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

mkdir -p "$dir" || die "$dir cannot be created"
[ -w "$dir" ] || die "$dir is not writable"

# One installer at a time, including a second invocation from another terminal.
# The lock directory is the product's own path; an unrecognised directory is never
# removed, and a dead owner's own lock is reclaimed only after its contents are
# re-read and match.
lock="$dir/.gewu-install.lock"
if ! mkdir "$lock" 2>/dev/null; then
    [ -d "$lock" ] || die "$dir is not writable"
    owner=$(cat "$lock/pid" 2>/dev/null || true)
    case "$owner" in
        ''|*[!0-9]*) die "another install is using $lock; if none is running, remove that directory" ;;
    esac
    if kill -0 "$owner" 2>/dev/null; then
        die "another install is already running (pid $owner)"
    fi
    [ "$(cat "$lock/token" 2>/dev/null || true)" = "gewu-install-lock-v1" ] \
        || die "refusing to remove unrecognised lock $lock"
    [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$owner" ] \
        || die "the lock changed while checking it; not touching $lock"
    rm -f "$lock/pid" "$lock/token" 2>/dev/null || die "the stale lock cannot be cleared; not touching $lock"
    rmdir "$lock" 2>/dev/null || die "another install started; not touching $lock"
    mkdir "$lock" 2>/dev/null || die "another install started"
fi
printf '%s\n' "$$" > "$lock/pid" || die "the lock cannot be written"
printf '%s\n' "gewu-install-lock-v1" > "$lock/token" || die "the lock cannot be written"

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
tmp=$(mktemp -d) || die "a temporary directory cannot be created"
trap cleanup EXIT

say "downloading $tarball ($tag)"
curl -fsSL -o "$tmp/$tarball" "$base/$tarball" || die "download failed"
curl -fsSL -o "$tmp/$tarball.sha256" "$base/$tarball.sha256" || die "checksum download failed"
( cd "$tmp" && sha256sum -c "$tarball.sha256" >/dev/null 2>&1 ) || die "checksum verification failed — refusing to install"

mkdir -p "$tmp/unpack"
tar -xzf "$tmp/$tarball" -C "$tmp/unpack" || die "the archive could not be unpacked"
bin=$(find "$tmp/unpack" -maxdepth 2 -type f -name gewu-multiverse | head -1)
[ -n "$bin" ] || die "the archive carries no gewu-multiverse binary"

# Check the new binary before replacing anything. A build that cannot run on this
# machine must never remove the build that can.
if ! "$bin" --version >/dev/null 2>&1; then
    say "install.sh: the downloaded binary did not run here — it needs glibc >= $floor; the installed version is unchanged"
    say "install.sh: build from source on this machine instead: cargo install --git https://github.com/$repo"
    exit 1
fi

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

# Install beside the old binary, then rename over it. The rename is atomic, so a
# failure before this line leaves the old binary and its version marker intact.
newbin="$dir/.gewu-multiverse.new.$$"
install -m 0755 "$bin" "$newbin" || die "the new binary cannot be staged; the installed version is unchanged"
mv -f "$newbin" "$dir/gewu-multiverse" || die "the new binary cannot be put in place; the installed version is unchanged"
newbin=
say "installed $dir/gewu-multiverse ($tag)"

on_path=0
case ":$PATH:" in
    *":$dir:"*) on_path=1 ;;
    *) say "note: $dir is not on PATH — add it with: export PATH=\"$dir:\$PATH\"" ;;
esac

# Run the mounted binary as a child of this script, so SIGINT/SIGTERM reaches both
# and the trap can cancel a download or a program install. The child's own signal
# handler writes no half-stage; the install journal recovers a hard kill.
run_product() {
    "$dir/gewu-multiverse" "$@" &
    child=$!
    if wait "$child"; then
        child=
        return 0
    fi
    status=$?
    child=
    return "$status"
}

# The product's own Forgejo is fetched now, once, so the first run does not have to
# wait for it. A failure is not fatal: the product is installed, and its first start
# tries again.
say "fetching Forgejo 17.0.0 for gewu-multiverse (about 122 MB; progress is shown below)"
# Keep the product's own progress on stderr: the 122 MB fetch is the longest step,
# and a silent line makes a slow connection look stuck (F7). Standard output is the
# product's ready line, which the installer replaces with its own.
if run_product --install-forgejo >/dev/null; then
    say "Forgejo 17.0.0 is ready"
else
    status=$?
    if [ "$status" -eq 130 ]; then
        say "install.sh: canceled; the product binary is installed and Forgejo is unchanged"
        exit 130
    fi
    say "the Forgejo download did not finish — the product is installed; its first start will try again"
fi

if [ -n "$harnesses" ] && [ "$harnesses" != "none" ]; then
    old_ifs=$IFS
    IFS=,
    for program in $harnesses; do
        IFS=$old_ifs
        [ -n "$program" ] || continue
        say "installing the $program agent program (official package, verified)"
        if run_product --install-harness "$program"; then
            say "the $program agent program is ready"
        else
            status=$?
            if [ "$status" -eq 130 ]; then
                say "install.sh: canceled the $program install; the previous version is kept"
                exit 130
            fi
            say "the $program agent program was not installed — the product is installed; choose Install on its Program screen to retry"
        fi
        IFS=,
    done
    IFS=$old_ifs
fi

# The closing line must be a command that resolves. When the directory is not on
# PATH, the bare name would print `command not found` on the user's first command
#.
if [ "$on_path" = 1 ]; then
    say "run it with: gewu-multiverse"
else
    say "run it with: $dir/gewu-multiverse"
fi
