#!/bin/sh

set -eu

umask 022
LC_ALL=C
export LC_ALL

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
VERIFY_SCRIPT="$SCRIPT_DIR/verify-riscv-specifications.sh"

MANUAL_NAME="riscv-isa-manual"
MANUAL_REPOSITORY="https://github.com/riscv/riscv-isa-manual.git"
MANUAL_TAG="riscv-isa-release-971bc71-2026-07-22"
MANUAL_COMMIT="971bc71be1429c97a6d3cc0c66a2bb2e6d5472ff"
MANUAL_TREE="798ec179363ed9fc9704e787c72623915227380c"
MANUAL_DESTINATION="riscv-isa-manual-v20260120"

SAIL_NAME="sail-riscv"
SAIL_REPOSITORY="https://github.com/riscv/sail-riscv.git"
SAIL_TAG="0.12"
SAIL_COMMIT="65ddde80ee2b131bf46c20e6e748343c336c4071"
SAIL_TREE="51c4d481d9e9ccf87aa98897cbad3c237efeb4e3"
SAIL_DESTINATION="sail-riscv-0.12"

DESTINATION="$REPOSITORY_ROOT/third_party/riscv"
STAGING_DIRECTORY=""
IMPORT_LOCK=""
IMPORT_LOCK_ACQUIRED=0
HASH_STYLE=""
LINK_HASH_INPUT=""
MANUAL_STAGE_PATH=""
MANUAL_TARGET_PATH=""
MANUAL_PUBLISH_STARTED=0
SAIL_STAGE_PATH=""
SAIL_TARGET_PATH=""
SAIL_PUBLISH_STARTED=0
LOCK_STAGE_PATH=""
LOCK_TARGET_PATH=""
LOCK_PUBLISH_STARTED=0
INVENTORY_STAGE_PATH=""
INVENTORY_TARGET_PATH=""
INVENTORY_PUBLISH_STARTED=0

usage() {
    cat <<EOF
Usage: $(basename "$0") [--destination DIRECTORY]

Import pinned RISC-V ISA manual and Sail model snapshots. DIRECTORY is the
RISC-V third-party root that will contain specifications/ and manifests/.
It defaults to:

  $REPOSITORY_ROOT/third_party/riscv

The command refuses to replace an existing snapshot or manifest.
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*"
}

path_exists() {
    [ -e "$1" ] || [ -L "$1" ]
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

    if [ "$status" -ne 0 ]; then
        rollback_published_entry \
            "$INVENTORY_PUBLISH_STARTED" \
            "$INVENTORY_TARGET_PATH" \
            "$INVENTORY_STAGE_PATH"
        rollback_published_entry \
            "$LOCK_PUBLISH_STARTED" \
            "$LOCK_TARGET_PATH" \
            "$LOCK_STAGE_PATH"
        rollback_published_entry \
            "$SAIL_PUBLISH_STARTED" \
            "$SAIL_TARGET_PATH" \
            "$SAIL_STAGE_PATH"
        rollback_published_entry \
            "$MANUAL_PUBLISH_STARTED" \
            "$MANUAL_TARGET_PATH" \
            "$MANUAL_STAGE_PATH"
    fi

    if [ -n "$STAGING_DIRECTORY" ] && [ -d "$STAGING_DIRECTORY" ]; then
        case "$STAGING_DIRECTORY" in
            "$DESTINATION"/.import-riscv-specifications.*)
                rm -rf "$STAGING_DIRECTORY"
                ;;
            *)
                printf 'warning: refusing to remove unexpected staging path: %s\n' \
                    "$STAGING_DIRECTORY" >&2
                ;;
        esac
    fi

    if [ "$IMPORT_LOCK_ACQUIRED" -eq 1 ] && [ -n "$IMPORT_LOCK" ]; then
        rmdir "$IMPORT_LOCK" 2>/dev/null || true
    fi

    exit "$status"
}

rollback_published_entry() {
    publish_started=$1
    published_path=$2
    staged_path=$3

    [ "$publish_started" -eq 1 ] || return 0

    if path_exists "$staged_path"; then
        if path_exists "$published_path"; then
            printf 'warning: both staged and published paths exist; refusing rollback: %s\n' \
                "$published_path" >&2
        fi
        return 0
    fi

    if ! path_exists "$published_path"; then
        printf 'warning: could not find published path during rollback: %s\n' \
            "$published_path" >&2
        return 0
    fi

    if ! mv "$published_path" "$staged_path"; then
        printf 'warning: could not roll back published path: %s\n' \
            "$published_path" >&2
    fi
}

clone_pinned_source() {
    source_name=$1
    source_repository=$2
    source_tag=$3
    source_commit=$4
    source_tree=$5
    source_directory=$6

    note "Fetching $source_name at $source_tag..."
    git clone \
        --quiet \
        --depth 1 \
        --single-branch \
        --no-checkout \
        --branch "$source_tag" \
        "$source_repository" \
        "$source_directory" ||
        die "failed to clone $source_name"

    actual_commit=$(git -C "$source_directory" rev-parse 'HEAD^{commit}') ||
        die "could not resolve the imported $source_name commit"
    [ "$actual_commit" = "$source_commit" ] ||
        die "$source_name tag resolved to $actual_commit, expected $source_commit"

    actual_tree=$(git -C "$source_directory" rev-parse 'HEAD^{tree}') ||
        die "could not resolve the imported $source_name tree"
    [ "$actual_tree" = "$source_tree" ] ||
        die "$source_name commit has tree $actual_tree, expected $source_tree"
}

export_manual() {
    source_directory=$1
    output_directory=$2
    archive_file=$3

    mkdir -p "$output_directory"
    git -C "$source_directory" archive \
        --format=tar \
        "$MANUAL_COMMIT" \
        -- \
        LICENSE \
        README.md \
        src/unpriv/preface.adoc \
        src/unpriv/rv32.adoc \
        src/unpriv/m-st-ext.adoc \
        src/unpriv/images \
        >"$archive_file" ||
        die "failed to archive selected RISC-V ISA manual sources"

    tar -xf "$archive_file" -C "$output_directory" ||
        die "failed to extract selected RISC-V ISA manual sources"
}

export_sail() {
    source_directory=$1
    output_directory=$2
    archive_file=$3

    mkdir -p "$output_directory"
    git -C "$source_directory" archive \
        --format=tar \
        "$SAIL_COMMIT" \
        -- \
        LICENCE \
        README.md \
        doc/ReadingGuide.md \
        model \
        >"$archive_file" ||
        die "failed to archive selected Sail sources"

    tar -xf "$archive_file" -C "$output_directory" ||
        die "failed to extract selected Sail sources"
}

write_lock_file() {
    lock_file=$1

    cat >"$lock_file" <<EOF
{
  "schema_version": 1,
  "sources": [
    {
      "name": "$MANUAL_NAME",
      "repository": "$MANUAL_REPOSITORY",
      "tag": "$MANUAL_TAG",
      "commit": "$MANUAL_COMMIT",
      "tree": "$MANUAL_TREE",
      "document_version": "20260120",
      "license": "CC-BY-4.0",
      "destination": "specifications/$MANUAL_DESTINATION",
      "imported_paths": [
        "LICENSE",
        "README.md",
        "src/unpriv/preface.adoc",
        "src/unpriv/rv32.adoc",
        "src/unpriv/m-st-ext.adoc",
        "src/unpriv/images"
      ],
      "contents_modified": false
    },
    {
      "name": "$SAIL_NAME",
      "repository": "$SAIL_REPOSITORY",
      "tag": "$SAIL_TAG",
      "commit": "$SAIL_COMMIT",
      "tree": "$SAIL_TREE",
      "model_version": "0.12",
      "license": "BSD-2-Clause",
      "destination": "specifications/$SAIL_DESTINATION",
      "imported_paths": [
        "LICENCE",
        "README.md",
        "doc/ReadingGuide.md",
        "model"
      ],
      "contents_modified": false
    }
  ]
}
EOF
}

assert_inventory_path() {
    relative_path=$1

    case "$relative_path" in
        "" | /* | */../* | ../* | */..)
            die "unsupported path in imported snapshot: $relative_path"
            ;;
    esac

    case "$relative_path" in
        *[!A-Za-z0-9._/@+-]*)
            die "non-portable path in imported snapshot: $relative_path"
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

write_inventory() {
    payload_root=$1
    inventory_file=$2
    unsorted_paths="$STAGING_DIRECTORY/inventory-paths.unsorted"
    sorted_paths="$STAGING_DIRECTORY/inventory-paths.sorted"

    (
        cd "$payload_root"
        find specifications \( -type f -o -type l \) -print
        printf '%s\n' "manifests/specifications.lock.json"
    ) >"$unsorted_paths"

    LC_ALL=C sort "$unsorted_paths" >"$sorted_paths"
    : >"$inventory_file"

    while IFS= read -r relative_path; do
        assert_inventory_path "$relative_path"
        absolute_path="$payload_root/$relative_path"

        if [ -L "$absolute_path" ]; then
            entry_kind="l"
        elif [ -f "$absolute_path" ] && [ -x "$absolute_path" ]; then
            entry_kind="x"
        elif [ -f "$absolute_path" ]; then
            entry_kind="f"
        else
            die "unsupported imported entry type: $relative_path"
        fi

        entry_hash=$(hash_imported_path "$absolute_path")
        printf '%s\t%s\t%s\n' \
            "$entry_kind" \
            "$entry_hash" \
            "$relative_path" \
            >>"$inventory_file"
    done <"$sorted_paths"
}

publish_payload() {
    payload_root=$1

    MANUAL_STAGE_PATH="$payload_root/specifications/$MANUAL_DESTINATION"
    MANUAL_TARGET_PATH="$DESTINATION/specifications/$MANUAL_DESTINATION"
    SAIL_STAGE_PATH="$payload_root/specifications/$SAIL_DESTINATION"
    SAIL_TARGET_PATH="$DESTINATION/specifications/$SAIL_DESTINATION"
    LOCK_STAGE_PATH="$payload_root/manifests/specifications.lock.json"
    LOCK_TARGET_PATH="$DESTINATION/manifests/specifications.lock.json"
    INVENTORY_STAGE_PATH="$payload_root/manifests/specifications.inventory.tsv"
    INVENTORY_TARGET_PATH="$DESTINATION/manifests/specifications.inventory.tsv"

    for target in \
        "$MANUAL_TARGET_PATH" \
        "$SAIL_TARGET_PATH" \
        "$LOCK_TARGET_PATH" \
        "$INVENTORY_TARGET_PATH"
    do
        path_exists "$target" &&
            die "refusing to replace existing import output: $target"
    done

    MANUAL_PUBLISH_STARTED=1
    mv "$MANUAL_STAGE_PATH" "$MANUAL_TARGET_PATH"
    SAIL_PUBLISH_STARTED=1
    mv "$SAIL_STAGE_PATH" "$SAIL_TARGET_PATH"
    LOCK_PUBLISH_STARTED=1
    mv "$LOCK_STAGE_PATH" "$LOCK_TARGET_PATH"
    INVENTORY_PUBLISH_STARTED=1
    mv "$INVENTORY_STAGE_PATH" "$INVENTORY_TARGET_PATH"
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

require_command git
require_command tar
require_command find
require_command sort
require_command mktemp
require_command readlink
[ -f "$VERIFY_SCRIPT" ] ||
    die "verification script not found: $VERIFY_SCRIPT"
select_hash_tool

if [ -L "$DESTINATION" ]; then
    die "destination must not be a symbolic link: $DESTINATION"
fi
mkdir -p "$DESTINATION"
DESTINATION=$(CDPATH= cd -- "$DESTINATION" && pwd -P)
[ "$DESTINATION" != "/" ] || die "refusing to import into the filesystem root"

for directory in \
    "$DESTINATION/specifications" \
    "$DESTINATION/manifests"
do
    if [ -L "$directory" ]; then
        die "import directory must not be a symbolic link: $directory"
    fi
    if path_exists "$directory" && [ ! -d "$directory" ]; then
        die "import path exists but is not a directory: $directory"
    fi
    mkdir -p "$directory"
done

for output in \
    "$DESTINATION/specifications/$MANUAL_DESTINATION" \
    "$DESTINATION/specifications/$SAIL_DESTINATION" \
    "$DESTINATION/manifests/specifications.lock.json" \
    "$DESTINATION/manifests/specifications.inventory.tsv"
do
    path_exists "$output" &&
        die "refusing to replace existing import output: $output"
done

IMPORT_LOCK="$DESTINATION/.import-riscv-specifications.lock"
mkdir "$IMPORT_LOCK" 2>/dev/null ||
    die "another import may be running; lock exists: $IMPORT_LOCK"
IMPORT_LOCK_ACQUIRED=1
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

STAGING_DIRECTORY=$(mktemp -d \
    "$DESTINATION/.import-riscv-specifications.XXXXXX") ||
    die "could not create an import staging directory"

PAYLOAD_ROOT="$STAGING_DIRECTORY/payload"
MANUAL_SOURCE="$STAGING_DIRECTORY/manual-source"
SAIL_SOURCE="$STAGING_DIRECTORY/sail-source"
LINK_HASH_INPUT="$STAGING_DIRECTORY/link-target"

mkdir -p "$PAYLOAD_ROOT/specifications" "$PAYLOAD_ROOT/manifests"

clone_pinned_source \
    "$MANUAL_NAME" \
    "$MANUAL_REPOSITORY" \
    "$MANUAL_TAG" \
    "$MANUAL_COMMIT" \
    "$MANUAL_TREE" \
    "$MANUAL_SOURCE"
clone_pinned_source \
    "$SAIL_NAME" \
    "$SAIL_REPOSITORY" \
    "$SAIL_TAG" \
    "$SAIL_COMMIT" \
    "$SAIL_TREE" \
    "$SAIL_SOURCE"

note "Exporting selected RISC-V ISA manual sources..."
export_manual \
    "$MANUAL_SOURCE" \
    "$PAYLOAD_ROOT/specifications/$MANUAL_DESTINATION" \
    "$STAGING_DIRECTORY/manual.tar"

note "Exporting Sail formal model sources..."
export_sail \
    "$SAIL_SOURCE" \
    "$PAYLOAD_ROOT/specifications/$SAIL_DESTINATION" \
    "$STAGING_DIRECTORY/sail.tar"

write_lock_file "$PAYLOAD_ROOT/manifests/specifications.lock.json"
write_inventory \
    "$PAYLOAD_ROOT" \
    "$PAYLOAD_ROOT/manifests/specifications.inventory.tsv"

note "Verifying staged import..."
sh "$VERIFY_SCRIPT" --destination "$PAYLOAD_ROOT"

publish_payload "$PAYLOAD_ROOT"

note "Verifying published import..."
sh "$VERIFY_SCRIPT" --destination "$DESTINATION"

note "Imported pinned RISC-V specifications into:"
note "  $DESTINATION"
