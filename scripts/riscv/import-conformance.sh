#!/bin/sh

set -eu

umask 022
LC_ALL=C
export LC_ALL

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
VERIFY_SCRIPT="$SCRIPT_DIR/verify-conformance.sh"

ACT4_NAME="riscv-arch-test"
ACT4_REPOSITORY="https://github.com/riscv/riscv-arch-test.git"
ACT4_BRANCH="act4"
ACT4_COMMIT="619aa16960e69ac29e9b558fb907babfb937a090"
ACT4_TREE="21abb5f6d10914892fc9af62cde555a01fd91d7a"
ACT4_OMITTED_GITLINK_PATH="docs/docs-resources"
ACT4_OMITTED_GITLINK_COMMIT="50d4c15c33e8e769c5291f36ab299d2b3aa019f4"
ACT4_DESTINATION="riscv-arch-test-act4-619aa169"
ACT4_EXPECTED_ENTRY_COUNT=316

RISCV_TESTS_NAME="riscv-tests"
RISCV_TESTS_REPOSITORY="https://github.com/riscv-software-src/riscv-tests.git"
RISCV_TESTS_BRANCH="master"
RISCV_TESTS_COMMIT="34e6b6d1e7936b526075432fb730d89148623484"
RISCV_TESTS_TREE="0bea492c255802d4c43da82b17a0043a4a1a256d"
RISCV_TESTS_OMITTED_GITLINK_PATH="env"
RISCV_TESTS_OMITTED_GITLINK_COMMIT="6de71edb142be36319e380ce782c3d1830c65d68"
RISCV_TESTS_DESTINATION="riscv-tests-34e6b6d1"
RISCV_TESTS_EXPECTED_ENTRY_COUNT=111

DESTINATION="$REPOSITORY_ROOT/third_party/riscv"
STAGING_DIRECTORY=""
IMPORT_LOCK=""
IMPORT_LOCK_ACQUIRED=0
HASH_STYLE=""
LINK_HASH_INPUT=""
UPSTREAM_STAGE_PATH=""
UPSTREAM_TARGET_PATH=""
UPSTREAM_PUBLISH_STARTED=0
LOCK_STAGE_PATH=""
LOCK_TARGET_PATH=""
LOCK_PUBLISH_STARTED=0
INVENTORY_STAGE_PATH=""
INVENTORY_TARGET_PATH=""
INVENTORY_PUBLISH_STARTED=0

usage() {
    cat <<EOF
Usage: $(basename "$0") [--destination DIRECTORY]

Import pinned ACT4 and riscv-tests source snapshots for RV32I/M. DIRECTORY is
the RISC-V third-party root that will contain conformance/ and manifests/. It
defaults to:

  $REPOSITORY_ROOT/third_party/riscv

The command requires network access and refuses to replace an existing
snapshot or manifest.
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
            "$UPSTREAM_PUBLISH_STARTED" \
            "$UPSTREAM_TARGET_PATH" \
            "$UPSTREAM_STAGE_PATH"
    fi

    if [ -n "$STAGING_DIRECTORY" ] && [ -d "$STAGING_DIRECTORY" ]; then
        case "$STAGING_DIRECTORY" in
            "$DESTINATION"/.import-riscv-conformance.*)
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

fetch_pinned_source() {
    source_name=$1
    source_repository=$2
    source_commit=$3
    source_tree=$4
    source_directory=$5

    note "Fetching $source_name at $source_commit..."
    mkdir "$source_directory" ||
        die "could not create source directory for $source_name"
    git -C "$source_directory" init --quiet ||
        die "could not initialize source repository for $source_name"
    git -C "$source_directory" remote add origin "$source_repository" ||
        die "could not configure source repository for $source_name"
    git -C "$source_directory" -c protocol.version=2 fetch \
        --quiet \
        --depth 1 \
        --filter=blob:none \
        --no-tags \
        origin \
        "$source_commit" ||
        die "failed to fetch pinned $source_name commit"

    actual_commit=$(git -C "$source_directory" rev-parse 'FETCH_HEAD^{commit}') ||
        die "could not resolve the fetched $source_name commit"
    [ "$actual_commit" = "$source_commit" ] ||
        die "$source_name fetch resolved to $actual_commit, expected $source_commit"

    actual_tree=$(git -C "$source_directory" rev-parse 'FETCH_HEAD^{tree}') ||
        die "could not resolve the fetched $source_name tree"
    [ "$actual_tree" = "$source_tree" ] ||
        die "$source_name commit has tree $actual_tree, expected $source_tree"
}

assert_omitted_gitlink() {
    source_name=$1
    source_directory=$2
    source_commit=$3
    gitlink_path=$4
    gitlink_commit=$5
    tab=$(printf '\t')

    actual_entry=$(git -C "$source_directory" ls-tree \
        "$source_commit" \
        -- \
        "$gitlink_path") ||
        die "could not inspect omitted $source_name gitlink: $gitlink_path"
    expected_entry="160000 commit $gitlink_commit${tab}$gitlink_path"
    [ "$actual_entry" = "$expected_entry" ] ||
        die "$source_name gitlink differs from the recorded pin: $gitlink_path"
}

export_act4() {
    source_directory=$1
    output_directory=$2
    archive_file=$3

    mkdir -p "$output_directory"
    git -C "$source_directory" archive \
        --format=tar \
        "$ACT4_COMMIT" \
        -- \
        COPYING.APACHE \
        COPYING.BSD \
        COPYING.CC \
        README.md \
        framework/pyproject.toml \
        framework/src/act \
        generators/testgen/pyproject.toml \
        generators/testgen/src/testgen \
        testplans/I.csv \
        testplans/M.csv \
        tests/env \
        tests/rv32i/I \
        tests/rv32i/M \
        >"$archive_file" ||
        die "failed to archive selected ACT4 sources"

    tar -xf "$archive_file" -C "$output_directory" ||
        die "failed to extract selected ACT4 sources"
}

export_riscv_tests() {
    source_directory=$1
    output_directory=$2
    archive_file=$3

    mkdir -p "$output_directory"
    git -C "$source_directory" archive \
        --format=tar \
        "$RISCV_TESTS_COMMIT" \
        -- \
        .gitmodules \
        LICENSE \
        README.md \
        isa/macros/scalar/test_macros.h \
        isa/rv32ui \
        isa/rv32um \
        isa/rv64ui \
        >"$archive_file" ||
        die "failed to archive selected riscv-tests sources"

    tar -xf "$archive_file" -C "$output_directory" ||
        die "failed to extract selected riscv-tests sources"
}

count_snapshot_entries() {
    snapshot_directory=$1
    find "$snapshot_directory" \( -type f -o -type l \) -print |
        wc -l |
        tr -d '[:space:]'
}

assert_snapshot_entry_count() {
    snapshot_name=$1
    snapshot_directory=$2
    expected_count=$3

    actual_count=$(count_snapshot_entries "$snapshot_directory") ||
        die "could not count imported $snapshot_name entries"
    [ "$actual_count" = "$expected_count" ] ||
        die "$snapshot_name import has $actual_count entries, expected $expected_count"
}

write_lock_file() {
    lock_file=$1

    cat >"$lock_file" <<EOF
{
  "schema_version": 1,
  "sources": [
    {
      "name": "$ACT4_NAME",
      "suite": "ACT4",
      "suite_kind": "architectural-certification-tests",
      "repository": "$ACT4_REPOSITORY",
      "branch_at_selection": "$ACT4_BRANCH",
      "commit": "$ACT4_COMMIT",
      "tree": "$ACT4_TREE",
      "commit_timestamp": "2026-07-22T04:43:39Z",
      "release_baseline": "4.0.0",
      "destination": "conformance/upstream/$ACT4_DESTINATION",
      "imported_paths": [
        "COPYING.APACHE",
        "COPYING.BSD",
        "COPYING.CC",
        "README.md",
        "framework/pyproject.toml",
        "framework/src/act",
        "generators/testgen/pyproject.toml",
        "generators/testgen/src/testgen",
        "testplans/I.csv",
        "testplans/M.csv",
        "tests/env",
        "tests/rv32i/I",
        "tests/rv32i/M"
      ],
      "expected_entry_count": $ACT4_EXPECTED_ENTRY_COUNT,
      "omitted_gitlinks": [
        {
          "path": "$ACT4_OMITTED_GITLINK_PATH",
          "commit": "$ACT4_OMITTED_GITLINK_COMMIT",
          "reason": "Documentation build dependency; not part of the RV32I/M test corpus."
        }
      ],
      "validated_downstream_generation": {
        "command": "testgen testplans -o generated --extensions I,M --jobs 1",
        "uv": "0.11.28",
        "sail_riscv": "0.12",
        "riscv_gnu_toolchain": "2026.07.15",
        "upstream_frozen_workspace_imported": false,
        "project_integration_required": true
      },
      "licenses": [
        "Apache-2.0",
        "BSD-3-Clause",
        "CC-BY-4.0"
      ],
      "contents_modified": false
    },
    {
      "name": "$RISCV_TESTS_NAME",
      "suite_kind": "legacy-isa-smoke-tests",
      "repository": "$RISCV_TESTS_REPOSITORY",
      "branch_at_selection": "$RISCV_TESTS_BRANCH",
      "commit": "$RISCV_TESTS_COMMIT",
      "tree": "$RISCV_TESTS_TREE",
      "commit_timestamp": "2026-06-03T16:03:02+08:00",
      "destination": "conformance/upstream/$RISCV_TESTS_DESTINATION",
      "imported_paths": [
        ".gitmodules",
        "LICENSE",
        "README.md",
        "isa/macros/scalar/test_macros.h",
        "isa/rv32ui",
        "isa/rv32um",
        "isa/rv64ui"
      ],
      "expected_entry_count": $RISCV_TESTS_EXPECTED_ENTRY_COUNT,
      "omitted_gitlinks": [
        {
          "path": "$RISCV_TESTS_OMITTED_GITLINK_PATH",
          "commit": "$RISCV_TESTS_OMITTED_GITLINK_COMMIT",
          "reason": "Privileged test environment; the benchmark supplies its own EEI adapter."
        }
      ],
      "license": "BSD-3-Clause",
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
        conformance/upstream/"$ACT4_DESTINATION"/*|conformance/upstream/"$RISCV_TESTS_DESTINATION"/*|manifests/conformance.lock.json)
            ;;
        *)
            die "path is outside the conformance snapshots: $relative_path"
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
        find conformance/upstream \( -type f -o -type l \) -print
        printf '%s\n' "manifests/conformance.lock.json"
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

    UPSTREAM_STAGE_PATH="$payload_root/conformance/upstream"
    UPSTREAM_TARGET_PATH="$DESTINATION/conformance/upstream"
    LOCK_STAGE_PATH="$payload_root/manifests/conformance.lock.json"
    LOCK_TARGET_PATH="$DESTINATION/manifests/conformance.lock.json"
    INVENTORY_STAGE_PATH="$payload_root/manifests/conformance.inventory.tsv"
    INVENTORY_TARGET_PATH="$DESTINATION/manifests/conformance.inventory.tsv"

    for target in \
        "$UPSTREAM_TARGET_PATH" \
        "$LOCK_TARGET_PATH" \
        "$INVENTORY_TARGET_PATH"
    do
        path_exists "$target" &&
            die "refusing to replace existing import output: $target"
    done

    UPSTREAM_PUBLISH_STARTED=1
    mv "$UPSTREAM_STAGE_PATH" "$UPSTREAM_TARGET_PATH"
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
require_command tr
require_command wc
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
    "$DESTINATION/conformance" \
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
    "$DESTINATION/conformance/upstream" \
    "$DESTINATION/manifests/conformance.lock.json" \
    "$DESTINATION/manifests/conformance.inventory.tsv"
do
    path_exists "$output" &&
        die "refusing to replace existing import output: $output"
done

IMPORT_LOCK="$DESTINATION/.import-riscv-conformance.lock"
mkdir "$IMPORT_LOCK" 2>/dev/null ||
    die "another import may be running; lock exists: $IMPORT_LOCK"
IMPORT_LOCK_ACQUIRED=1
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

STAGING_DIRECTORY=$(mktemp -d \
    "$DESTINATION/.import-riscv-conformance.XXXXXX") ||
    die "could not create an import staging directory"

PAYLOAD_ROOT="$STAGING_DIRECTORY/payload"
ACT4_SOURCE="$STAGING_DIRECTORY/act4-source"
RISCV_TESTS_SOURCE="$STAGING_DIRECTORY/riscv-tests-source"
LINK_HASH_INPUT="$STAGING_DIRECTORY/link-target"

mkdir -p "$PAYLOAD_ROOT/conformance/upstream" "$PAYLOAD_ROOT/manifests"

fetch_pinned_source \
    "$ACT4_NAME" \
    "$ACT4_REPOSITORY" \
    "$ACT4_COMMIT" \
    "$ACT4_TREE" \
    "$ACT4_SOURCE"
assert_omitted_gitlink \
    "$ACT4_NAME" \
    "$ACT4_SOURCE" \
    "$ACT4_COMMIT" \
    "$ACT4_OMITTED_GITLINK_PATH" \
    "$ACT4_OMITTED_GITLINK_COMMIT"

fetch_pinned_source \
    "$RISCV_TESTS_NAME" \
    "$RISCV_TESTS_REPOSITORY" \
    "$RISCV_TESTS_COMMIT" \
    "$RISCV_TESTS_TREE" \
    "$RISCV_TESTS_SOURCE"
assert_omitted_gitlink \
    "$RISCV_TESTS_NAME" \
    "$RISCV_TESTS_SOURCE" \
    "$RISCV_TESTS_COMMIT" \
    "$RISCV_TESTS_OMITTED_GITLINK_PATH" \
    "$RISCV_TESTS_OMITTED_GITLINK_COMMIT"

note "Exporting selected ACT4 RV32I/M sources..."
export_act4 \
    "$ACT4_SOURCE" \
    "$PAYLOAD_ROOT/conformance/upstream/$ACT4_DESTINATION" \
    "$STAGING_DIRECTORY/act4.tar"
assert_snapshot_entry_count \
    "$ACT4_NAME" \
    "$PAYLOAD_ROOT/conformance/upstream/$ACT4_DESTINATION" \
    "$ACT4_EXPECTED_ENTRY_COUNT"

note "Exporting selected riscv-tests RV32I/M sources..."
export_riscv_tests \
    "$RISCV_TESTS_SOURCE" \
    "$PAYLOAD_ROOT/conformance/upstream/$RISCV_TESTS_DESTINATION" \
    "$STAGING_DIRECTORY/riscv-tests.tar"
assert_snapshot_entry_count \
    "$RISCV_TESTS_NAME" \
    "$PAYLOAD_ROOT/conformance/upstream/$RISCV_TESTS_DESTINATION" \
    "$RISCV_TESTS_EXPECTED_ENTRY_COUNT"

write_lock_file "$PAYLOAD_ROOT/manifests/conformance.lock.json"
write_inventory \
    "$PAYLOAD_ROOT" \
    "$PAYLOAD_ROOT/manifests/conformance.inventory.tsv"

note "Verifying staged import..."
sh "$VERIFY_SCRIPT" --destination "$PAYLOAD_ROOT"

publish_payload "$PAYLOAD_ROOT"

note "Verifying published import..."
sh "$VERIFY_SCRIPT" --destination "$DESTINATION"

note "Imported pinned RISC-V test-suite sources into:"
note "  $DESTINATION"
