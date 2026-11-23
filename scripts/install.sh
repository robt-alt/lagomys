#!/bin/sh
set -eu

fail() {
    printf 'lagomys installer: %s\n' "$1" >&2
    exit 1
}

valid_tag() {
    case $1 in
        ''|*[!v0-9.]*) return 1 ;;
    esac
    printf '%s\n' "$1" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'
}

latest_failed() {
    printf '%s\n' \
        'Unable to resolve the latest GitHub release. If the API is unavailable or rate limited, pin the version.' \
        "  curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/robt-alt/lagomys/main/scripts/install.sh | LAGOMYS_VERSION=v0.2.0 sh" >&2
    exit 1
}

fetch() {
    case $1 in
        http://*) set -- --proto '=http,https' --noproxy '*' -- "$1" ;;
        *) set -- --proto '=https' -- "$1" ;;
    esac
    curl -q --proto-redir '=https' --tlsv1.2 -fsSL --globoff "$@"
}

cleanup() {
    if [ -n "$staged" ]; then
        rm -f -- "$staged"
    fi
    if [ -n "$scratch_dir" ]; then
        rm -rf -- "$scratch_dir"
    fi
}

release_tag=${LAGOMYS_VERSION-}
if [ "${LAGOMYS_VERSION+x}" = x ] && ! valid_tag "$release_tag"; then
    fail 'Invalid LAGOMYS_VERSION. Use a tag such as v0.2.0.'
fi
command -v curl >/dev/null 2>&1 || fail 'curl is required.'
if command -v sha256sum >/dev/null 2>&1; then
    set -- sha256sum
elif command -v shasum >/dev/null 2>&1; then
    set -- shasum -a 256
else
    fail 'sha256sum or shasum is required.'
fi

system=$(uname -s)
architecture=$(uname -m)
case "$system:$architecture" in
    Linux:x86_64) platform=linux-x86_64 ;;
    Linux:aarch64|Linux:arm64) platform=linux-arm64 ;;
    Darwin:arm64) platform=macos-arm64 ;;
    *) fail "Unsupported platform $system $architecture." ;;
esac

download_base=${LAGOMYS_DOWNLOAD_BASE:-https://github.com/robt-alt/lagomys/releases/download}
download_base=${download_base%/}
case "$download_base" in
    https://*) ;;
    http://*)
        authority=${download_base#http://}
        authority=${authority%%/*}
        case "$authority" in
            *[[:space:]]*) fail 'HTTP is allowed only for loopback downloads.' ;;
        esac
        if ! printf '%s\n' "$authority" |
            LC_ALL=C grep -Eq '^(localhost|127\.0\.0\.1|\[::1\])(:[0-9]+)?$'; then
            fail 'HTTP is allowed only for loopback downloads.'
        fi
        ;;
    *) fail 'LAGOMYS_DOWNLOAD_BASE must use HTTPS or loopback HTTP.' ;;
esac

if [ -z "$release_tag" ]; then
    release=$(fetch https://api.github.com/repos/robt-alt/lagomys/releases/latest) || latest_failed
    release_tag=$(printf '%s\n' "$release" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    valid_tag "$release_tag" || latest_failed
fi

staged=
scratch_dir=
trap cleanup 0
trap 'exit 1' HUP INT TERM
scratch_dir=$(mktemp -d "${TMPDIR:-/tmp}/lagomys.XXXXXXXX") ||
    fail 'Could not create a temporary download directory.'

package="lagomys-$release_tag-$platform"
archive="$package.tar.gz"
release_url="$download_base/$release_tag"
fetch "$release_url/$archive" >"$scratch_dir/$archive" || fail "Could not download $archive."
fetch "$release_url/SHA256SUMS" >"$scratch_dir/SHA256SUMS" || fail 'Could not download SHA256SUMS.'

expected_hash=$(LC_ALL=C awk -v archive="$archive" '
    { sub(/\r$/, "") }
    NF == 2 && ($2 == archive || $2 == "*" archive) { checksum = $1; count++ }
    END {
        if (count != 1 || length(checksum) != 64 || checksum ~ /[^0-9a-fA-F]/) exit 1
        print tolower(checksum)
    }
' "$scratch_dir/SHA256SUMS") || fail "SHA256SUMS must contain one valid checksum for $archive."
computed_hash=$("$@" <"$scratch_dir/$archive") || fail "Could not hash $archive."
[ "${computed_hash%% *}" = "$expected_hash" ] || fail "SHA256 mismatch for $archive."

tar -xOzf "$scratch_dir/$archive" "$package/lagomys" >"$scratch_dir/lagomys" ||
    fail "Could not extract lagomys from $archive."
[ -s "$scratch_dir/lagomys" ] || fail "$archive contains an empty lagomys binary."

bin_dir=${LAGOMYS_BIN_DIR:-"${HOME:?HOME is required when LAGOMYS_BIN_DIR is unset.}/.local/bin"}
case "$bin_dir" in
    /*) ;;
    *) bin_dir="$PWD/$bin_dir" ;;
esac
mkdir -p "$bin_dir" || fail 'Could not create the installation directory.'
destination="$bin_dir/lagomys"
[ ! -d "$destination" ] || fail 'The installation destination is a directory.'
staged=$(mktemp "$bin_dir/.lagomys.XXXXXXXX") || fail 'Could not stage the new binary.'
cp "$scratch_dir/lagomys" "$staged" || fail 'Could not copy the new binary.'
chmod 755 "$staged" || fail 'Could not make the new binary executable.'
# Check the candidate before replacing an existing installation.
version_output=$("$staged" --version) || fail 'The downloaded binary could not run.'
mv -f "$staged" "$destination" || fail 'Could not replace the installed binary.'
staged=

printf '%s\n' "$version_output"
case ":${PATH-}:" in
    *":$bin_dir:"*) ;;
    *)
        quoted_bin_dir=$(printf '%s' "$bin_dir" | sed "s/'/'\\\\''/g")
        printf "Add the installation directory to PATH.\n  export PATH='%s':\"\$PATH\"\n" "$quoted_bin_dir"
        ;;
esac
