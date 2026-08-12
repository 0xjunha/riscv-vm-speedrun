#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
bucket=${GCP_SELECTED_RESULTS_BUCKET:-gs://selected-harbor-results}
destination=${GCP_SELECTED_RESULTS_DESTINATION:-$repository/jobs/gcp-harbor-selected}

mkdir -p "$destination"
temporary_dir=$(mktemp -d "$destination/.sync.XXXXXX")
trap 'rm -rf "$temporary_dir"' 0
trap 'exit 1' HUP INT TERM

gcloud storage objects list "$bucket/**" \
    --format='value(name,generation)' >"$temporary_dir/objects"

tab=$(printf '\t')
while IFS="$tab" read -r object generation; do
    [ -n "$object" ] || continue
    case "$object" in
        *.tgz) ;;
        *) echo "Skipping non-archive object: $object"; continue ;;
    esac
    case "$object" in
        /* | *//* | *[!A-Za-z0-9._/-]*)
            echo "unsafe selected-results object name: $object" >&2
            exit 2
            ;;
    esac
    case "/$object/" in
        */../* | */./*)
            echo "unsafe selected-results object name: $object" >&2
            exit 2
            ;;
    esac
    case "$generation" in
        '' | *[!0-9]*)
            echo "invalid generation for selected-results object: $object" >&2
            exit 2
            ;;
    esac
    relative=${object%.tgz}
    extracted=$destination/$relative
    marker=$extracted/.gcs-generation
    if [ ! -L "$extracted" ] && [ -f "$marker" ] && \
        [ "$(cat "$marker")" = "$generation" ]; then
        rm -f "$destination/$object"
        echo "Unchanged: $relative"
        continue
    fi

    archive=$temporary_dir/archive.tgz
    staging=$temporary_dir/extracted
    backup=$temporary_dir/backup
    rm -rf "$archive" "$staging" "$backup"

    echo "Downloading: $object"
    gcloud storage cp "$bucket/$object" "$archive"
    mkdir "$staging"
    python3 -c 'import sys, tarfile; tarfile.open(sys.argv[1], "r:gz").extractall(sys.argv[2], filter="data")' \
        "$archive" "$staging"

    printf '%s\n' "$generation" >"$staging/.gcs-generation"
    mkdir -p "$(dirname -- "$extracted")"
    [ ! -e "$extracted" ] && [ ! -L "$extracted" ] || mv "$extracted" "$backup"
    if ! mv "$staging" "$extracted"; then
        [ ! -e "$backup" ] || mv "$backup" "$extracted"
        exit 1
    fi

    rm -rf "$backup"
    rm -f "$destination/$object"
    echo "Extracted: $relative"
done <"$temporary_dir/objects"

echo "Selected Harbor results: $destination"
