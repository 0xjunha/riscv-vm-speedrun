#!/bin/sh

set -eu

LC_ALL=C
export LC_ALL

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
DESTINATION="$REPOSITORY_ROOT/third_party/riscv"
HASH_STYLE=""
TEMPORARY_DIRECTORY=""
TEMPORARY_PARENT=""
LINK_HASH_INPUT=""

usage() {
    cat <<EOF
Usage: $(basename "$0") [--destination DIRECTORY]

Verify the exact contents, entry types, and executable bits of imported RISC-V
specification snapshots. DIRECTORY defaults to:

  $REPOSITORY_ROOT/third_party/riscv
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 ||
        die "required command not found: $1"
}

select_hash_tool() {
    if command -v sha256sum >/dev/null 2>&1; then
        HASH_STYLE="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        HASH_STYLE="shasum"
    else
        die "required SHA-256 tool not found (install sha256sum or shasum)"
    fi
}

sha256_file() {
    hash_output=""

    case "$HASH_STYLE" in
        sha256sum)
            hash_output=$(sha256sum <"$1") ||
                die "could not hash $1"
            ;;
        shasum)
            hash_output=$(shasum -a 256 <"$1") ||
                die "could not hash $1"
            ;;
        *)
            die "internal error: SHA-256 tool was not selected"
            ;;
    esac

    printf '%s\n' "${hash_output%% *}"
}

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM

    if [ -n "$TEMPORARY_DIRECTORY" ] && [ -d "$TEMPORARY_DIRECTORY" ]; then
        case "$TEMPORARY_DIRECTORY" in
            "$TEMPORARY_PARENT"/verify-riscv-specifications.*)
                rm -rf "$TEMPORARY_DIRECTORY"
                ;;
            *)
                printf 'warning: refusing to remove unexpected temporary path: %s\n' \
                    "$TEMPORARY_DIRECTORY" >&2
                ;;
        esac
    fi

    exit "$status"
}

assert_inventory_path() {
    relative_path=$1

    case "$relative_path" in
        "" | /* | */../* | ../* | */..)
            die "unsupported path in inventory: $relative_path"
            ;;
    esac

    case "$relative_path" in
        specifications/README.md)
            die "project-owned README must not appear in the upstream inventory"
            ;;
        specifications/* | manifests/specifications.lock.json)
            ;;
        *)
            die "inventory path is outside the specification snapshot: $relative_path"
            ;;
    esac

    case "$relative_path" in
        *[!A-Za-z0-9._/@+-]*)
            die "non-portable path in inventory: $relative_path"
            ;;
    esac
}

hash_imported_path() {
    absolute_path=$1

    if [ -L "$absolute_path" ]; then
        link_target=$(readlink "$absolute_path") ||
            die "could not read symbolic link: $absolute_path"
        printf '%s' "$link_target" >"$LINK_HASH_INPUT"
        sha256_file "$LINK_HASH_INPUT"
    else
        sha256_file "$absolute_path"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --destination)
            [ "$#" -ge 2 ] || die "--destination requires a directory"
            DESTINATION=$2
            shift 2
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (run with --help for usage)"
            ;;
    esac
done

require_command find
require_command sort
require_command diff
require_command grep
require_command uniq
require_command mktemp
require_command readlink
select_hash_tool

[ -d "$DESTINATION" ] ||
    die "RISC-V third-party root does not exist: $DESTINATION"
[ ! -L "$DESTINATION" ] ||
    die "RISC-V third-party root must not be a symbolic link: $DESTINATION"
DESTINATION=$(CDPATH= cd -- "$DESTINATION" && pwd -P)

INVENTORY="$DESTINATION/manifests/specifications.inventory.tsv"
LOCK_FILE="$DESTINATION/manifests/specifications.lock.json"
[ -f "$INVENTORY" ] || die "inventory not found: $INVENTORY"
[ ! -L "$INVENTORY" ] || die "inventory must not be a symbolic link: $INVENTORY"
[ -f "$LOCK_FILE" ] || die "source lock not found: $LOCK_FILE"
[ ! -L "$LOCK_FILE" ] || die "source lock must not be a symbolic link: $LOCK_FILE"
[ -d "$DESTINATION/specifications" ] ||
    die "specification directory not found: $DESTINATION/specifications"
[ ! -L "$DESTINATION/specifications" ] ||
    die "specification directory must not be a symbolic link"

SPECIFICATIONS_README="$DESTINATION/specifications/README.md"
if [ -e "$SPECIFICATIONS_README" ] || [ -L "$SPECIFICATIONS_README" ]; then
    [ -f "$SPECIFICATIONS_README" ] && [ ! -L "$SPECIFICATIONS_README" ] ||
        die "project-owned specification README must be a regular file"
    [ ! -x "$SPECIFICATIONS_README" ] ||
        die "project-owned specification README must not be executable"
fi

TEMPORARY_PARENT=${TMPDIR:-/tmp}
[ -d "$TEMPORARY_PARENT" ] ||
    die "temporary directory does not exist: $TEMPORARY_PARENT"
TEMPORARY_PARENT=$(CDPATH= cd -- "$TEMPORARY_PARENT" && pwd -P)
TEMPORARY_DIRECTORY=$(mktemp -d \
    "$TEMPORARY_PARENT/verify-riscv-specifications.XXXXXX") ||
    die "could not create a verification directory"
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

LINK_HASH_INPUT="$TEMPORARY_DIRECTORY/link-target"
EXPECTED_PATHS="$TEMPORARY_DIRECTORY/expected-paths"
EXPECTED_PATHS_SORTED="$TEMPORARY_DIRECTORY/expected-paths.sorted"
ACTUAL_PATHS="$TEMPORARY_DIRECTORY/actual-paths"
ACTUAL_PATHS_SORTED="$TEMPORARY_DIRECTORY/actual-paths.sorted"
SPECIAL_PATHS="$TEMPORARY_DIRECTORY/special-paths"
DUPLICATE_PATHS="$TEMPORARY_DIRECTORY/duplicate-paths"
: >"$EXPECTED_PATHS"

tab=$(printf '\t')
entry_count=0
while IFS="$tab" read -r expected_kind expected_hash relative_path extra_field; do
    [ -n "$expected_kind" ] || die "inventory contains a blank line"
    [ -z "${extra_field:-}" ] ||
        die "inventory contains too many fields for: $relative_path"
    case "$expected_kind" in
        f | x | l)
            ;;
        *)
            die "unknown inventory entry type: $expected_kind"
            ;;
    esac
    printf '%s\n' "$expected_hash" | grep -Eq '^[0-9a-f]{64}$' ||
        die "invalid SHA-256 digest for: $relative_path"
    assert_inventory_path "$relative_path"

    absolute_path="$DESTINATION/$relative_path"
    case "$expected_kind" in
        l)
            [ -L "$absolute_path" ] ||
                die "expected symbolic link: $relative_path"
            ;;
        x)
            [ -f "$absolute_path" ] && [ ! -L "$absolute_path" ] ||
                die "expected regular file: $relative_path"
            [ -x "$absolute_path" ] ||
                die "expected executable file: $relative_path"
            ;;
        f)
            [ -f "$absolute_path" ] && [ ! -L "$absolute_path" ] ||
                die "expected regular file: $relative_path"
            [ ! -x "$absolute_path" ] ||
                die "unexpected executable bit: $relative_path"
            ;;
    esac

    actual_hash=$(hash_imported_path "$absolute_path")
    [ "$actual_hash" = "$expected_hash" ] ||
        die "SHA-256 mismatch: $relative_path"

    printf '%s\n' "$relative_path" >>"$EXPECTED_PATHS"
    entry_count=$((entry_count + 1))
done <"$INVENTORY"

[ "$entry_count" -gt 0 ] || die "inventory is empty"

LC_ALL=C sort "$EXPECTED_PATHS" >"$EXPECTED_PATHS_SORTED"
uniq -d "$EXPECTED_PATHS_SORTED" >"$DUPLICATE_PATHS"
if [ -s "$DUPLICATE_PATHS" ]; then
    IFS= read -r duplicate_path <"$DUPLICATE_PATHS" || true
    die "duplicate inventory path: $duplicate_path"
fi

(
    cd "$DESTINATION"
    find specifications \
        \( -type f -o -type l \) \
        ! -path "specifications/README.md" \
        -print
    printf '%s\n' "manifests/specifications.lock.json"
) >"$ACTUAL_PATHS"
LC_ALL=C sort "$ACTUAL_PATHS" >"$ACTUAL_PATHS_SORTED"

(
    cd "$DESTINATION"
    find specifications \
        ! -type d \
        ! -type f \
        ! -type l \
        -print
) >"$SPECIAL_PATHS"
if [ -s "$SPECIAL_PATHS" ]; then
    IFS= read -r special_path <"$SPECIAL_PATHS" || true
    die "unsupported special entry found: $special_path"
fi

if ! diff -u "$EXPECTED_PATHS_SORTED" "$ACTUAL_PATHS_SORTED"; then
    die "imported path set differs from the recorded inventory"
fi

printf 'Verified %s imported RISC-V specification entries in:\n' "$entry_count"
printf '  %s\n' "$DESTINATION"
