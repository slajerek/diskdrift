#!/usr/bin/env bash

set -Eeuo pipefail

SCRIPT_DIR=$(dirname -- "${BASH_SOURCE[0]}")
SCRIPT_DIR=$(cd -- "$SCRIPT_DIR" && pwd -P)
readonly SCRIPT_DIR
readonly CHANNEL=unstable
readonly RETRIES=${PUBLISH_RETRIES:-3}
readonly RETRY_DELAY=${PUBLISH_RETRY_DELAY:-1}

REPO=
CANDIDATE_SHA=
ASSETS_DIR=
DRY_RUN=0
FAULT_AFTER=
WORK_DIR=
API_COUNTER=0
API_BODY=
API_ETAG=
HASH_COMMAND=
MUTATION_STARTED=0
SIGNAL_HANDLING=0
DEFERRED_SIGNAL=
CONCURRENT_CONFLICT=0
ROLLBACK_CONFLICT=0

TAG_EXISTS=0
TAG_TYPE=
TAG_SHA=
TAG_ETAG=
TAG_MUTATION_PENDING=0
RELEASE_EXISTS=0
RELEASE_ID=
RELEASE_UPLOAD_URL=
RELEASE_ETAG=
RELEASE_TOUCHED=0
RELEASE_FIELDS=
BACKUP_DIR=
declare -a OLD_ASSET_IDS=()
declare -a OLD_ASSET_METADATA=()
BASELINE_TAG_FILE=
BASELINE_RELEASE_FILE=
BASELINE_ASSETS_FILE=
TRANSACTION_TAG_FILE=
TRANSACTION_RELEASE_FILE=
TRANSACTION_ASSETS_FILE=
MUTATION_ASSET_ID=
MUTATION_ASSET_NAME=
MUTATION_ASSET_BYTES=
MUTATION_ASSET_METADATA=
MUTATION_RELEASE_FILE=
CURRENT_TAG_ETAG=
CURRENT_RELEASE_ETAG=

usage() {
    cat >&2 <<'EOF'
usage: scripts/publish-unstable.sh --repo OWNER/REPO --sha SHA --assets-dir DIR [--dry-run] [--fault-after STAGE]
EOF
    exit 2
}

candidate_error() {
    printf 'candidate validation failed: %s\n' "$*" >&2
    exit 3
}

publication_error() {
    local rollback_status publication_status
    printf 'publication failed: %s\n' "$*" >&2
    SIGNAL_HANDLING=1
    if rollback; then
        rollback_status=0
    else
        rollback_status=$?
    fi
    if [ "$rollback_status" -ne 0 ]; then
        if [ "$ROLLBACK_CONFLICT" -eq 1 ] || [ "$CONCURRENT_CONFLICT" -eq 1 ]; then
            printf 'CONCURRENT CONFLICT: rollback refused to overwrite another actor\n' >&2
            publication_status=6
        else
            printf 'ROLLBACK FAILED: the tag and release may be partially restored\n' >&2
            publication_status=5
        fi
    elif [ "$CONCURRENT_CONFLICT" -ne 0 ]; then
        publication_status=6
    else
        publication_status=4
    fi
    if [ -n "$DEFERRED_SIGNAL" ]; then
        printf 'signal %s received during rollback; preserving publication status %s\n' \
            "$DEFERRED_SIGNAL" "$publication_status" >&2
    fi
    exit "$publication_status"
}

cleanup() {
    if [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
        rm -rf -- "$WORK_DIR"
    fi
}

on_signal() {
    local signal=$1
    local signal_status
    if [ "$SIGNAL_HANDLING" -eq 1 ]; then
        DEFERRED_SIGNAL=$signal
        return 0
    fi
    if [ "$MUTATION_STARTED" -eq 1 ] && [ "$SIGNAL_HANDLING" -eq 0 ]; then
        SIGNAL_HANDLING=1
        trap - HUP INT TERM
        if rollback; then
            cleanup
            case "$signal" in
                HUP) signal_status=129 ;;
                INT) signal_status=130 ;;
                TERM) signal_status=143 ;;
            esac
            exit "$signal_status"
        fi
        cleanup
        if [ "$ROLLBACK_CONFLICT" -eq 1 ] || [ "$CONCURRENT_CONFLICT" -eq 1 ]; then
            printf 'CONCURRENT CONFLICT: rollback refused to overwrite another actor after signal %s\n' "$signal" >&2
            exit 6
        fi
        printf 'ROLLBACK FAILED after signal %s\n' "$signal" >&2
        exit 5
    fi
    cleanup
    case "$signal" in
        HUP) exit 129 ;;
        INT) exit 130 ;;
        TERM) exit 143 ;;
    esac
}

trap cleanup EXIT
trap 'on_signal HUP' HUP
trap 'on_signal INT' INT
trap 'on_signal TERM' TERM

log() {
    printf '%s\n' "$*" >&2
}

retry_sleep() {
    if [ "$RETRY_DELAY" != 0 ]; then
        sleep "$RETRY_DELAY"
    fi
}

lowercase() {
    tr 'A-F' 'a-f'
}

hash_file() {
    if [ "$HASH_COMMAND" = sha256sum ]; then
        sha256sum -- "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

hash_available() {
    if command -v sha256sum >/dev/null 2>&1; then
        HASH_COMMAND=sha256sum
        return 0
    fi
    if command -v shasum >/dev/null 2>&1; then
        HASH_COMMAND=shasum
        return 0
    fi
    return 1
}

fault_after() {
    if [ "$FAULT_AFTER" = "$1" ]; then
        printf 'injected failure after %s\n' "$1" >&2
        return 1
    fi
    return 0
}

require_etag() {
    if [ -n "$1" ]; then
        return 0
    fi
    CONCURRENT_CONFLICT=1
    log 'GitHub response did not provide a mutation ETag; refusing to mutate'
    return 1
}

has_stage() {
    case "$1" in
        before-tag-mutation|before-release-mutation|tag-create|tag-update|release-create|release-update|old-asset-delete|candidate-upload|tag-rollback|release-rollback)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

json_body() {
    local response=$1
    local body=$2
    local http_status

    http_status=$(awk '/^HTTP\// { status=$2 } END { print status }' "$response")
    API_ETAG=$(awk 'tolower($1) == "etag:" {
        value=$0
        sub(/^[^:]*:[[:space:]]*/, "", value)
        sub(/\r$/, "", value)
    } END { print value }' "$response")
    if [ -z "$http_status" ]; then
        cp -- "$response" "$body"
        return 0
    fi

    awk 'BEGIN { body = 0 } body { print; next } /^\r?$/ { body = 1; next }' \
        "$response" > "$body"
    API_HTTP_STATUS=$http_status
}

api_request() {
    local operation=$1
    local method=$2
    local endpoint=$3
    shift 3
    local attempt response error http_status rc

    API_COUNTER=$((API_COUNTER + 1))
    response="$WORK_DIR/api-response-$API_COUNTER"
    error="$WORK_DIR/api-error-$API_COUNTER"
    API_BODY="$WORK_DIR/api-body-$API_COUNTER"

    attempt=1
    while [ "$attempt" -le "$RETRIES" ]; do
        : > "$response"
        : > "$error"
        API_HTTP_STATUS=
        API_ETAG=
        if PUBLISH_OPERATION="$operation" gh api --method "$method" --include "$@" \
            "$endpoint" >"$response" 2>"$error"; then
            rc=0
        else
            rc=$?
        fi

        json_body "$response" "$API_BODY" || return 1
        http_status=${API_HTTP_STATUS:-}
        if [ -s "$API_BODY" ] && ! jq -e . "$API_BODY" >/dev/null 2>&1; then
            printf 'malformed JSON response for %s\n' "$operation" > "$error"
            if reconcile_operation "$operation"; then
                return 0
            fi
            if [ "$attempt" -lt "$RETRIES" ]; then
                retry_sleep
                attempt=$((attempt + 1))
                continue
            fi
            cat "$error" >&2
            return 1
        fi
        if [ -n "$http_status" ] && [ "$http_status" -ge 200 ] && [ "$http_status" -lt 300 ]; then
            return 0
        fi
        if [ "$rc" -eq 0 ] && [ -z "$http_status" ]; then
            return 0
        fi
        if [ "$http_status" = 404 ] && [ "$method" = DELETE ]; then
            return 0
        fi
        if [ "$http_status" = 412 ] || [ "$http_status" = 428 ]; then
            CONCURRENT_CONFLICT=1
            printf 'conditional mutation rejected for %s\n' "$operation" >&2
            return 1
        fi
        if [ "$http_status" = 404 ]; then
            return 44
        fi

        if reconcile_operation "$operation"; then
            return 0
        fi

        if [ "$attempt" -lt "$RETRIES" ]; then
            retry_sleep
        fi
        attempt=$((attempt + 1))
    done

    if [ -s "$error" ]; then
        cat "$error" >&2
    elif [ -s "$API_BODY" ]; then
        cat "$API_BODY" >&2
    fi
    return 1
}

download_asset() {
    local operation=$1
    local url=$2
    local output=$3
    local attempt rc

    attempt=1
    while [ "$attempt" -le "$RETRIES" ]; do
        if PUBLISH_OPERATION="$operation" gh api --method GET \
            --header 'Accept: application/octet-stream' "$url" >"$output" 2>"$WORK_DIR/download-error"; then
            return 0
        else
            rc=$?
        fi
        if [ "$attempt" -lt "$RETRIES" ]; then
            retry_sleep
        fi
        attempt=$((attempt + 1))
    done
    cat "$WORK_DIR/download-error" >&2
    return "$rc"
}

upload_asset() {
    local path=$1
    local name=$2
    local content_type=$3
    local asset_id
    MUTATION_ASSET_ID=
    MUTATION_ASSET_NAME=$name
    MUTATION_ASSET_BYTES=$path
    require_etag "$RELEASE_ETAG" || return 1
    if ! api_request candidate-asset-upload POST \
        "$RELEASE_UPLOAD_URL" \
        --header "If-Match: $RELEASE_ETAG" \
        --header "Content-Type: $content_type" \
        --raw-field "name=$name" --raw-field "label=" --input "$path"; then
        return 1
    fi
    refresh_release_etag || return 1
    asset_id=$(jq -r '.id // empty' "$API_BODY")
    [ -n "$asset_id" ] || return 1
    MUTATION_ASSET_ID=$asset_id
}

verify_asset_backup() {
    local metadata=$1
    local bytes=$2
    local expected_size expected_digest actual_size actual_digest

    if ! jq -e '(.size | type == "number" and floor == .) and
        ((.digest == null) or (.digest | type == "string" and test("^(sha256:)?[0-9a-fA-F]{64}$")))' \
        "$metadata" >/dev/null; then
        return 1
    fi
    expected_size=$(jq -r '.size' "$metadata")
    expected_digest=$(jq -r '.digest // empty' "$metadata")
    expected_digest=${expected_digest#sha256:}
    actual_size=$(wc -c < "$bytes" | tr -d '[:space:]')
    actual_digest=$(hash_file "$bytes")
    [ "$actual_size" = "$expected_size" ] || return 1
    [ -z "$expected_digest" ] || [ "$(printf '%s' "$actual_digest" | lowercase)" = "$(printf '%s' "$expected_digest" | lowercase)" ]
}

verify_restored_asset() {
    local metadata=$1
    local bytes=$2
    local response=$3
    local expected_name expected_label expected_type expected_size expected_digest response_digest
    local actual_size actual_digest

    expected_name=$(jq -r '.name' "$metadata")
    expected_label=$(jq -c 'if has("label") then .label else null end' "$metadata")
    expected_type=$(jq -r '.content_type // "application/octet-stream"' "$metadata")
    expected_size=$(jq -r '.size' "$metadata")
    expected_digest=$(jq -r '.digest // empty' "$metadata")
    expected_digest=${expected_digest#sha256:}
    actual_size=$(wc -c < "$bytes" | tr -d '[:space:]')
    actual_digest=$(hash_file "$bytes")
    [ "$actual_size" = "$expected_size" ] || return 1
    response_digest=$(jq -r '.digest // empty' "$response")
    response_digest=${response_digest#sha256:}
    if [ -n "$expected_digest" ]; then
        [ "$(printf '%s' "$actual_digest" | lowercase)" = "$(printf '%s' "$expected_digest" | lowercase)" ] || return 1
    fi
    if [ -n "$response_digest" ]; then
        [ "$(printf '%s' "$actual_digest" | lowercase)" = "$(printf '%s' "$response_digest" | lowercase)" ] || return 1
    fi
    jq -e --arg name "$expected_name" --arg type "$expected_type" \
        --arg size "$expected_size" \
        --argjson label "$expected_label" \
        '.name == $name and ((.label // null) == $label) and .content_type == $type and
         (.size | tostring) == $size' \
        "$response" >/dev/null
}

set_release_upload_url() {
    RELEASE_UPLOAD_URL=$(jq -r '.upload_url // empty' "$API_BODY")
    [ -n "$RELEASE_UPLOAD_URL" ] || return 1
    case "$RELEASE_UPLOAD_URL" in
        *'{'*) RELEASE_UPLOAD_URL=${RELEASE_UPLOAD_URL%%\{*} ;;
    esac
    [ -n "$RELEASE_UPLOAD_URL" ]
}

refresh_release_etag() {
    local preserved_body=$API_BODY
    api_request release-etag-refresh GET "repos/$REPO/releases/$RELEASE_ID" || return 1
    RELEASE_ETAG=$API_ETAG
    require_etag "$RELEASE_ETAG" || return 1
    API_BODY=$preserved_body
}

validate_manifest_and_archive() {
    local runner=$1
    local manifest="$ASSETS_DIR/manifest-$runner.json"
    local target binary archive expected_archive manifest_sha actual_sha

    if [ ! -f "$manifest" ] || [ -L "$manifest" ]; then
        candidate_error "missing regular manifest-$runner.json"
    fi

    if ! jq -e \
        --arg source_sha "$CANDIDATE_SHA" --arg expected_runner "$runner" '
        type == "object" and
        ((keys | sort) == ["binary", "runner", "rust_target", "schema", "sha256", "source_sha"]) and
        .schema == 1 and
        (.source_sha == $source_sha) and
        (.runner == $expected_runner) and
        (.rust_target | type == "string") and
        (.binary | type == "string") and
        (.sha256 | type == "string" and test("^[0-9a-fA-F]{64}$"))
    ' "$manifest" >/dev/null; then
        candidate_error "invalid manifest-$runner.json"
    fi

    target=$(jq -r '.rust_target' "$manifest")
    binary=$(jq -r '.binary' "$manifest")
    manifest_sha=$(jq -r '.sha256' "$manifest" | tr 'A-F' 'a-f')
    if [[ "$binary" == */* || "$binary" == .* || "$binary" == *$'\n'* ]]; then
        candidate_error "manifest-$runner.json has an unsafe binary name"
    fi

    case "$runner" in
        macos)
            [[ "$target" =~ ^(aarch64|x86_64)-apple-darwin$ ]] || \
                candidate_error "macOS manifest has an invalid Rust target"
            expected_archive="diskdrift-macos-$target.tar.gz"
            [ "$binary" = "diskdrift-macos-$target" ] || \
                candidate_error "macOS manifest binary disagrees with its target"
            ;;
        linux)
            [ "$target" = x86_64-unknown-linux-gnu ] || \
                candidate_error "Linux manifest has an invalid Rust target"
            expected_archive="diskdrift-linux-$target.tar.gz"
            [ "$binary" = "diskdrift-linux-$target" ] || \
                candidate_error "Linux manifest binary disagrees with its target"
            ;;
        windows)
            [ "$target" = x86_64-pc-windows-msvc ] || \
                candidate_error "Windows manifest has an invalid Rust target"
            expected_archive="diskdrift-windows-$target.zip"
            [ "$binary" = "diskdrift-windows-$target.exe" ] || \
                candidate_error "Windows manifest binary disagrees with its target"
            ;;
    esac

    archive="$ASSETS_DIR/$expected_archive"
    [ -f "$archive" ] || candidate_error "missing $expected_archive"
    [ ! -L "$archive" ] || candidate_error "$expected_archive is a symlink"
    actual_sha=$(hash_file "$archive")
    [ "$actual_sha" = "$manifest_sha" ] || \
        candidate_error "$expected_archive does not match manifest-$runner.json"

    case "$runner" in
        macos|linux)
            if ! archive_listing=$(tar -tzf "$archive"); then
                candidate_error "could not read $expected_archive"
            fi
            archive_count=0
            archive_entry=
            while IFS= read -r archive_line; do
                archive_count=$((archive_count + 1))
                archive_entry=$archive_line
            done <<EOF
$archive_listing
EOF
            [ "$archive_count" -eq 1 ] || \
                candidate_error "$expected_archive must contain exactly one file"
            [ "$archive_entry" = "$binary" ] || \
                candidate_error "$expected_archive contains an unexpected path"
            if ! tar -tvzf "$archive" | awk -v binary="$binary" \
                'NF >= 6 && $1 ~ /^-/ && $NF == binary { count++ } END { exit !(count == 1) }'; then
                candidate_error "$expected_archive does not contain one regular binary"
            fi
            ;;
        windows)
            if ! archive_listing=$(unzip -Z1 "$archive"); then
                candidate_error "could not read $expected_archive"
            fi
            archive_count=0
            archive_entry=
            while IFS= read -r archive_line; do
                archive_count=$((archive_count + 1))
                archive_entry=$archive_line
            done <<EOF
$archive_listing
EOF
            [ "$archive_count" -eq 1 ] || \
                candidate_error "$expected_archive must contain exactly one file"
            [ "$archive_entry" = "$binary" ] || \
                candidate_error "$expected_archive contains an unexpected path"
            if ! zipinfo -l "$archive" | awk -v binary="$binary" \
                '$1 ~ /^-/ && $NF == binary { count++ } END { exit !(count == 1) }'; then
                candidate_error "$expected_archive does not contain one regular binary"
            fi
            ;;
    esac
}

validate_candidate() {
    local entry base entries entry_count

    [ -d "$ASSETS_DIR" ] || candidate_error "assets directory does not exist"
    if ! entries=$(find -P "$ASSETS_DIR" -mindepth 1 -maxdepth 1 -print); then
        candidate_error "could not inspect assets directory"
    fi
    entry_count=0
    while IFS= read -r entry; do
        entry_count=$((entry_count + 1))
        base=${entry##*/}
        [ -f "$entry" ] && [ ! -L "$entry" ] || candidate_error "$base is not a regular non-symlink file"
        case "$base" in
            manifest-macos.json|manifest-linux.json|manifest-windows.json|diskdrift-macos-*.tar.gz|diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz|diskdrift-windows-x86_64-pc-windows-msvc.zip)
                ;;
            *)
                candidate_error "unexpected candidate file $base"
                ;;
        esac
    done <<EOF
$entries
EOF
    [ "$entry_count" -eq 6 ] || candidate_error "assets directory must contain exactly six files"

    validate_manifest_and_archive macos
    validate_manifest_and_archive linux
    validate_manifest_and_archive windows

    : > "$WORK_DIR/SHA256SUMS"
    for archive in \
        "$ASSETS_DIR"/diskdrift-macos-*.tar.gz \
        "$ASSETS_DIR"/diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz \
        "$ASSETS_DIR"/diskdrift-windows-x86_64-pc-windows-msvc.zip; do
        printf '%s  %s\n' "$(hash_file "$archive")" "${archive##*/}" >> "$WORK_DIR/SHA256SUMS"
    done
    while IFS='  ' read -r expected_hash archive_name; do
        [ -n "$archive_name" ] || continue
        actual_hash=$(hash_file "$ASSETS_DIR/$archive_name")
        [ "$(printf '%s' "$actual_hash" | lowercase)" = "$(printf '%s' "$expected_hash" | lowercase)" ] || \
            candidate_error "generated SHA256SUMS did not verify"
    done < "$WORK_DIR/SHA256SUMS"
}

run_public_tree_check() {
    local checkout=$PWD checkout_sha
    [ -x "$SCRIPT_DIR/check-public-tree.sh" ] || candidate_error "public-tree check is unavailable"
    if ! checkout_sha=$(git -C "$checkout" rev-parse HEAD 2>/dev/null); then
        candidate_error "checked-out candidate is not a Git revision"
    fi
    [ "$checkout_sha" = "$CANDIDATE_SHA" ] || \
        candidate_error "checked-out candidate does not match the requested SHA"
    if ! "$SCRIPT_DIR/check-public-tree.sh" "$checkout" >/dev/null; then
        candidate_error "checked-out candidate failed the public-tree check"
    fi
}

inventory() {
    local release_asset_count asset_id asset_url metadata bytes_sha

    OLD_ASSET_IDS=()
    OLD_ASSET_METADATA=()

    if api_request inventory-tag GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
        TAG_EXISTS=1
        TAG_TYPE=$(jq -r '.object.type // empty' "$API_BODY")
        TAG_SHA=$(jq -r '.object.sha // empty' "$API_BODY")
        TAG_ETAG=$API_ETAG
        [ -n "$TAG_TYPE" ] && [ -n "$TAG_SHA" ] || return 1
        jq -S '{object: .object}' "$API_BODY" > "$WORK_DIR/inventory-tag.json"
    else
        [ "$?" -eq 44 ] || return 1
        TAG_EXISTS=0
        TAG_ETAG=
        printf '%s\n' null > "$WORK_DIR/inventory-tag.json"
    fi

    if api_request inventory-release GET "repos/$REPO/releases/tags/$CHANNEL"; then
        RELEASE_EXISTS=1
        RELEASE_ID=$(jq -r '.id // empty' "$API_BODY")
        RELEASE_ETAG=$API_ETAG
        [ -n "$RELEASE_ID" ] || return 1
        set_release_upload_url || return 1
        RELEASE_FIELDS="$WORK_DIR/release-fields.json"
        release_fields_from_json "$API_BODY" > "$RELEASE_FIELDS"
        mkdir -p "$BACKUP_DIR/assets"
        if ! api_paginated_assets inventory-assets "$RELEASE_ID"; then
            return 1
        fi
        cp -- "$API_BODY" "$WORK_DIR/original-assets.json"
        release_asset_count=$(jq 'length' "$API_BODY")
        asset_index=0
        while [ "$asset_index" -lt "$release_asset_count" ]; do
            asset_id=$(jq -r ".[$asset_index].id | tostring" "$API_BODY")
            asset_url=$(jq -r ".[$asset_index].url // .[$asset_index].browser_download_url // empty" "$API_BODY")
            [ -n "$asset_id" ] && [ -n "$asset_url" ] || return 1
            metadata="$BACKUP_DIR/assets/$asset_index.json"
            jq ".[$asset_index]" "$API_BODY" > "$metadata"
            OLD_ASSET_IDS+=("$asset_id")
            OLD_ASSET_METADATA+=("$metadata")
            if ! download_asset backup-asset "$asset_url" "$BACKUP_DIR/assets/$asset_index.bytes"; then
                return 1
            fi
            if ! verify_asset_backup "$metadata" "$BACKUP_DIR/assets/$asset_index.bytes"; then
                log "backup verification failed for release asset $asset_id"
                return 1
            fi
            bytes_sha=$(hash_file "$BACKUP_DIR/assets/$asset_index.bytes")
            jq --arg bytes_sha "$bytes_sha" '. + {bytes_sha256: $bytes_sha}' "$metadata" > "$metadata.with-bytes"
            mv -- "$metadata.with-bytes" "$metadata"
            jq --arg id "$asset_id" --arg bytes_sha "$bytes_sha" \
                'map(if ((.id | tostring) == $id) then . + {bytes_sha256: $bytes_sha} else . end)' \
                "$WORK_DIR/original-assets.json" > "$WORK_DIR/original-assets.next"
            mv -- "$WORK_DIR/original-assets.next" "$WORK_DIR/original-assets.json"
            asset_index=$((asset_index + 1))
        done
    else
        [ "$?" -eq 44 ] || return 1
        RELEASE_EXISTS=0
        RELEASE_ETAG=
        printf '%s\n' '[]' > "$WORK_DIR/original-assets.json"
    fi

    BASELINE_TAG_FILE="$WORK_DIR/baseline-tag.json"
    BASELINE_RELEASE_FILE="$WORK_DIR/baseline-release.json"
    BASELINE_ASSETS_FILE="$WORK_DIR/baseline-assets.json"
    cp -- "$WORK_DIR/inventory-tag.json" "$BASELINE_TAG_FILE"
    if [ "$RELEASE_EXISTS" -eq 1 ]; then
        jq --arg id "$RELEASE_ID" '. + {release_id: $id}' "$RELEASE_FIELDS" > "$BASELINE_RELEASE_FILE"
        jq -S 'sort_by(.id | tostring)' "$WORK_DIR/original-assets.json" > "$BASELINE_ASSETS_FILE"
    else
        printf '%s\n' null > "$BASELINE_RELEASE_FILE"
        printf '%s\n' '[]' > "$BASELINE_ASSETS_FILE"
    fi
}

release_fields_from_json() {
    jq -S '["tag_name", "name", "body", "draft", "prerelease", "target_commitish", "make_latest", "discussion_category_name", "generate_release_notes"] as $fields |
        with_entries(select(.key as $key | $fields | index($key) != null))' "$1"
}

inventory_state() {
    local tag_file=$1
    local release_file=$2
    local assets_file=$3
    local operation_prefix=${4:-state}
    local status release_id

    CURRENT_TAG_ETAG=
    CURRENT_RELEASE_ETAG=

    if api_request "$operation_prefix-inventory-tag" GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
        CURRENT_TAG_ETAG=$API_ETAG
        jq -S '{object: .object}' "$API_BODY" > "$tag_file"
    else
        status=$?
        [ "$status" -eq 44 ] || return 1
        printf '%s\n' null > "$tag_file"
    fi

    if api_request "$operation_prefix-inventory-release" GET "repos/$REPO/releases/tags/$CHANNEL"; then
        CURRENT_RELEASE_ETAG=$API_ETAG
        release_id=$(jq -r '.id // empty' "$API_BODY")
        [ -n "$release_id" ] || return 1
        release_fields_from_json "$API_BODY" > "$release_file.fields"
        jq --arg id "$release_id" '. + {release_id: $id}' "$release_file.fields" > "$release_file"
        if ! api_paginated_assets "$operation_prefix-inventory-assets" "$release_id"; then
            return 1
        fi
        attach_asset_bytes_hashes "$API_BODY" "$assets_file" || return 1
    else
        status=$?
        [ "$status" -eq 44 ] || return 1
        printf '%s\n' null > "$release_file"
        printf '%s\n' '[]' > "$assets_file"
    fi
}

concurrent_conflict() {
    CONCURRENT_CONFLICT=1
    log 'concurrent release or tag mutation detected; refusing to overwrite it'
    return 1
}

asset_contracts() {
    # Compare only fields the publisher can restore; GitHub-owned IDs, URLs, and timestamps are excluded.
    jq -S '
        def contract: {
            name,
            label: (if has("label") then .label else null end),
            content_type: (.content_type // "application/octet-stream"),
            size,
            digest: (.digest // null),
            bytes_sha256: (.bytes_sha256 // null)
        };
        map(contract) |
        sort_by([.name, (.label | tostring), .content_type, (.size | tostring), (.digest | tostring), (.bytes_sha256 | tostring)])
    ' "$1"
}

attach_asset_bytes_hashes() {
    local source=$1
    local destination=$2
    local asset_count asset_index asset_id asset_url bytes_file bytes_sha

    cp -- "$source" "$destination"
    asset_count=$(jq 'length' "$destination")
    asset_index=0
    while [ "$asset_index" -lt "$asset_count" ]; do
        asset_id=$(jq -r ".[$asset_index].id | tostring" "$destination")
        asset_url=$(jq -r ".[$asset_index].url // .[$asset_index].browser_download_url // empty" "$destination")
        [ -n "$asset_url" ] || return 1
        bytes_file="$WORK_DIR/state-asset-$asset_index.bytes"
        download_asset state-asset-download "$asset_url" "$bytes_file" || return 1
        bytes_sha=$(hash_file "$bytes_file")
        jq --arg id "$asset_id" --arg bytes_sha "$bytes_sha" \
            'map(if ((.id | tostring) == $id) then . + {bytes_sha256: $bytes_sha} else . end)' \
            "$destination" > "$WORK_DIR/state-assets.next"
        mv -- "$WORK_DIR/state-assets.next" "$destination"
        asset_index=$((asset_index + 1))
    done
    jq -S 'sort_by(.id | tostring)' "$destination" > "$WORK_DIR/state-assets.sorted"
    mv -- "$WORK_DIR/state-assets.sorted" "$destination"
}

state_matches() {
    local expected_tag=$1
    local expected_release=$2
    local expected_assets=$3
    local current_tag="$WORK_DIR/current-tag.json"
    local current_release="$WORK_DIR/current-release.json"
    local current_assets="$WORK_DIR/current-assets.json"
    local expected_contract="$WORK_DIR/expected-assets-contract.json"
    local current_contract="$WORK_DIR/current-assets-contract.json"

    inventory_state "$current_tag" "$current_release" "$current_assets" || return 1
    asset_contracts "$expected_assets" > "$expected_contract"
    asset_contracts "$current_assets" > "$current_contract"
    cmp -s "$expected_tag" "$current_tag" &&
        cmp -s "$expected_release" "$current_release" &&
        cmp -s "$expected_contract" "$current_contract"
}

check_expected_state() {
    if ! state_matches "$1" "$2" "$3"; then
        concurrent_conflict
        return 1
    fi
    TAG_ETAG=$CURRENT_TAG_ETAG
    RELEASE_ETAG=$CURRENT_RELEASE_ETAG
    return 0
}

check_expected_release_state() {
    local expected_release=$1
    local expected_assets=$2
    local current_tag="$WORK_DIR/current-tag.json"
    local current_release="$WORK_DIR/current-release.json"
    local current_assets="$WORK_DIR/current-assets.json"
    local expected_contract="$WORK_DIR/expected-release-assets-contract.json"
    local current_contract="$WORK_DIR/current-release-assets-contract.json"

    if ! inventory_state "$current_tag" "$current_release" "$current_assets" rollback; then
        return 1
    fi
    asset_contracts "$expected_assets" > "$expected_contract"
    asset_contracts "$current_assets" > "$current_contract"
    if ! cmp -s "$expected_release" "$current_release" || \
        ! cmp -s "$expected_contract" "$current_contract"; then
        ROLLBACK_CONFLICT=1
        log 'release rollback refused to overwrite state changed by another actor'
        return 1
    fi
    RELEASE_ETAG=$CURRENT_RELEASE_ETAG
    return 0
}

record_transaction_state() {
    local tag_source=$1
    local release_source=$2
    local assets_source=$3

    TRANSACTION_TAG_FILE="$WORK_DIR/transaction-tag.json"
    TRANSACTION_RELEASE_FILE="$WORK_DIR/transaction-release.json"
    TRANSACTION_ASSETS_FILE="$WORK_DIR/transaction-assets.json"
    cp -- "$tag_source" "$TRANSACTION_TAG_FILE"
    cp -- "$release_source" "$TRANSACTION_RELEASE_FILE"
    cp -- "$assets_source" "$TRANSACTION_ASSETS_FILE"
}

api_paginated_assets() {
    local operation=$1
    local release_id=$2
    local attempt response error rc

    API_COUNTER=$((API_COUNTER + 1))
    response="$WORK_DIR/api-assets-response-$API_COUNTER"
    error="$WORK_DIR/api-assets-error-$API_COUNTER"
    API_BODY="$WORK_DIR/api-assets-body-$API_COUNTER"
    rc=1
    attempt=1
    while [ "$attempt" -le "$RETRIES" ]; do
        : > "$response"
        : > "$error"
        if PUBLISH_OPERATION="$operation" gh api --method GET --paginate --slurp \
            "repos/$REPO/releases/$release_id/assets?per_page=100" >"$response" 2>"$error"; then
            if jq -e 'type == "array" and all(.[]; type == "array")' "$response" >/dev/null 2>&1 && \
                jq 'add' "$response" > "$API_BODY"; then
                return 0
            fi
            printf 'malformed paginated asset response for %s\n' "$operation" > "$error"
        else
            rc=$?
        fi
        if [ "$attempt" -lt "$RETRIES" ]; then
            retry_sleep
        fi
        attempt=$((attempt + 1))
    done
    cat "$error" >&2
    return "$rc"
}

asset_response_matches() {
    local metadata=$1
    local response=$2
    local remote_url remote_bytes

    if ! verify_restored_asset "$metadata" "$MUTATION_ASSET_BYTES" "$response"; then
        return 1
    fi
    if [ -z "$(jq -r '.digest // empty' "$response")" ]; then
        remote_url=$(jq -r '.url // .browser_download_url // empty' "$response")
        [ -n "$remote_url" ] || return 1
        remote_bytes="$WORK_DIR/reconciled-asset.bytes"
        download_asset reconcile-asset-download "$remote_url" "$remote_bytes" || return 1
        cmp -s "$MUTATION_ASSET_BYTES" "$remote_bytes" || return 1
    fi
}

reconcile_asset_upload() {
    local assets_file candidate asset_id
    assets_file="$WORK_DIR/reconcile-assets.json"
    api_paginated_assets reconcile-asset-inventory "$RELEASE_ID" || return 1
    cp -- "$API_BODY" "$assets_file"
    candidate=$(jq -c --arg name "$MUTATION_ASSET_NAME" \
        --arg size "$(wc -c < "$MUTATION_ASSET_BYTES" | tr -d '[:space:]')" \
        'map(select(.name == $name and ((.size | tostring) == $size))) | .[0] // empty' \
        "$assets_file")
    [ -n "$candidate" ] || return 1
    printf '%s\n' "$candidate" > "$API_BODY"
    asset_id=$(jq -r '.id // empty' "$API_BODY")
    [ -n "$asset_id" ] || return 1
    asset_response_matches "$MUTATION_ASSET_METADATA" "$API_BODY" || return 1
    MUTATION_ASSET_ID=$asset_id
}

reconcile_operation() {
    local operation=$1
    local current_sha current_type release_id asset_id status
    case "$operation" in
        tag-create|tag-update)
            if api_request reconcile-tag GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
                current_sha=$(jq -r '.object.sha // empty' "$API_BODY")
                [ "$current_sha" = "$CANDIDATE_SHA" ] || return 1
                jq -S '{object: .object}' "$API_BODY" > "$WORK_DIR/reconciled-tag.json"
                return 0
            fi
            return 1
            ;;
        release-create)
            if api_request reconcile-release GET "repos/$REPO/releases/tags/$CHANNEL"; then
                release_id=$(jq -r '.id // empty' "$API_BODY")
                [ -n "$release_id" ] || return 1
                release_fields_from_json "$API_BODY" > "$WORK_DIR/reconciled-release.json"
                cmp -s "$MUTATION_RELEASE_FILE" "$WORK_DIR/reconciled-release.json" || return 1
                RELEASE_ID=$release_id
                set_release_upload_url || return 1
                return 0
            fi
            return 1
            ;;
        release-update)
            if api_request reconcile-release GET "repos/$REPO/releases/$RELEASE_ID"; then
                release_fields_from_json "$API_BODY" > "$WORK_DIR/reconciled-release.json"
                cmp -s "$MUTATION_RELEASE_FILE" "$WORK_DIR/reconciled-release.json"
                return $?
            fi
            return 1
            ;;
        old-asset-delete|release-rollback-asset-delete)
            if api_paginated_assets reconcile-asset-inventory "$RELEASE_ID"; then
                jq -e --arg id "$MUTATION_ASSET_ID" \
                    'all(.[]; ((.id | tostring) != $id))' "$API_BODY" >/dev/null
                return $?
            fi
            return 1
            ;;
        candidate-asset-upload|release-rollback-asset-upload)
            reconcile_asset_upload
            return $?
            ;;
        tag-rollback-update)
            if api_request reconcile-tag GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
                current_sha=$(jq -r '.object.sha // empty' "$API_BODY")
                current_type=$(jq -r '.object.type // empty' "$API_BODY")
                [ "$current_sha" = "$TAG_SHA" ] && [ "$current_type" = "$TAG_TYPE" ]
                return $?
            fi
            return 1
            ;;
        tag-rollback-delete)
            if api_request reconcile-tag GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
                return 1
            fi
            status=$?
            [ "$status" -eq 44 ]
            return $?
            ;;
        release-rollback)
            if api_request reconcile-release GET "repos/$REPO/releases/$RELEASE_ID"; then
                return 1
            fi
            status=$?
            [ "$status" -eq 44 ]
            return $?
            ;;
        release-rollback-update)
            if api_request reconcile-release GET "repos/$REPO/releases/$RELEASE_ID"; then
                release_fields_from_json "$API_BODY" > "$WORK_DIR/reconciled-release.json"
                cmp -s "$MUTATION_RELEASE_FILE" "$WORK_DIR/reconciled-release.json"
                return $?
            fi
            return 1
            ;;
        *)
            return 1
            ;;
    esac
}

check_candidate_remote() {
    local phase=${1:-candidate}
    local live_sha
    api_request "$phase-devel" GET "repos/$REPO/git/ref/heads/devel" || return 1
    live_sha=$(jq -r '.object.sha // empty' "$API_BODY")
    [ "$live_sha" = "$CANDIDATE_SHA" ] || {
        log "candidate SHA $CANDIDATE_SHA is not the live devel SHA $live_sha"
        return 1
    }

    api_request "$phase-merged-pr" GET "repos/$REPO/commits/$CANDIDATE_SHA/pulls?base=devel" || return 1
    jq -e 'any(.[]; .base.ref == "devel" and (.merged_at != null))' "$API_BODY" >/dev/null || {
        log "candidate SHA is not evidenced by a merged pull request into devel"
        return 1
    }
}

mutate_tag() {
    local payload
    if [ "$TAG_EXISTS" -eq 1 ]; then
        payload="$WORK_DIR/tag-update.json"
        jq -n --arg sha "$CANDIDATE_SHA" '{sha: $sha, force: true}' > "$payload"
        TAG_MUTATION_PENDING=1
        require_etag "$TAG_ETAG" || return 1
        api_request tag-update PATCH "repos/$REPO/git/refs/tags/$CHANNEL" \
            --header "If-Match: $TAG_ETAG" --input "$payload" || return 1
        TAG_ETAG=$API_ETAG
        fault_after tag-update || return 1
    else
        payload="$WORK_DIR/tag-create.json"
        jq -n --arg ref "refs/tags/$CHANNEL" --arg sha "$CANDIDATE_SHA" \
            '{ref: $ref, sha: $sha}' > "$payload"
        TAG_MUTATION_PENDING=1
        api_request tag-create POST "repos/$REPO/git/refs" \
            --header 'If-None-Match: *' --input "$payload" || return 1
        TAG_ETAG=$API_ETAG
        fault_after tag-create || return 1
    fi
}

transaction_remove_asset() {
    local asset_id=$1
    jq -S --arg id "$asset_id" 'map(select((.id | tostring) != $id)) | sort_by(.id | tostring)' \
        "$TRANSACTION_ASSETS_FILE" > "$WORK_DIR/transaction-assets.next"
    mv -- "$WORK_DIR/transaction-assets.next" "$TRANSACTION_ASSETS_FILE"
}

transaction_add_asset() {
    local asset_file=$1
    local bytes_file=${2:-}
    if [ -n "$bytes_file" ]; then
        jq -S --arg bytes_sha "$(hash_file "$bytes_file")" --slurpfile asset "$asset_file" \
            '. + [($asset[0] + {bytes_sha256: $bytes_sha})] | sort_by(.id | tostring)' \
            "$TRANSACTION_ASSETS_FILE" > "$WORK_DIR/transaction-assets.next"
    else
        jq -S --slurpfile asset "$asset_file" '. + [$asset[0]] | sort_by(.id | tostring)' \
            "$TRANSACTION_ASSETS_FILE" > "$WORK_DIR/transaction-assets.next"
    fi
    mv -- "$WORK_DIR/transaction-assets.next" "$TRANSACTION_ASSETS_FILE"
}

release_payload() {
    jq -n --arg tag "$CHANNEL" --arg title "DiskDrift unstable" \
        --arg body "Unstable development release for devel commit $CANDIDATE_SHA." \
        '{tag_name: $tag, name: $title, body: $body, draft: false, prerelease: true,
          generate_release_notes: false}'
}

mutate_release() {
    local payload
    if [ "$RELEASE_EXISTS" -eq 1 ]; then
        RELEASE_TOUCHED=1
        require_etag "$RELEASE_ETAG" || return 1
        payload="$WORK_DIR/release-update.json"
        release_payload > "$payload"
        jq 'del(.tag_name)' "$payload" > "$WORK_DIR/release-update-no-tag.json"
        jq -s '.[0] * .[1]' "$RELEASE_FIELDS" "$payload" > "$WORK_DIR/release-merged.json"
        release_fields_from_json "$WORK_DIR/release-merged.json" > "$WORK_DIR/release-expected-fields.json"
        MUTATION_RELEASE_FILE="$WORK_DIR/release-expected-fields.json"
        if ! api_request release-update PATCH "repos/$REPO/releases/$RELEASE_ID" \
            --header "If-Match: $RELEASE_ETAG" \
            --input "$WORK_DIR/release-update-no-tag.json"; then
            return 1
        fi
        RELEASE_ETAG=$API_ETAG
        release_fields_from_json "$API_BODY" > "$WORK_DIR/transaction-release-fields.json"
        jq --arg id "$RELEASE_ID" '. + {release_id: $id}' "$WORK_DIR/transaction-release-fields.json" > "$TRANSACTION_RELEASE_FILE"
        fault_after release-update || return 1
    else
        payload="$WORK_DIR/release-create.json"
        release_payload > "$payload"
        release_fields_from_json "$payload" > "$WORK_DIR/release-expected-fields.json"
        MUTATION_RELEASE_FILE="$WORK_DIR/release-expected-fields.json"
        RELEASE_TOUCHED=1
        if ! api_request release-create POST "repos/$REPO/releases" \
            --header 'If-None-Match: *' --input "$payload"; then
            return 1
        fi
        RELEASE_ID=$(jq -r '.id // empty' "$API_BODY")
        [ -n "$RELEASE_ID" ] || return 1
        set_release_upload_url || return 1
        RELEASE_ETAG=$API_ETAG
        release_fields_from_json "$API_BODY" > "$WORK_DIR/transaction-release-fields.json"
        jq --arg id "$RELEASE_ID" '. + {release_id: $id}' "$WORK_DIR/transaction-release-fields.json" > "$TRANSACTION_RELEASE_FILE"
        fault_after release-create || return 1
    fi
}

delete_old_assets() {
    local asset_id asset_index=0
    while [ "$asset_index" -lt "${#OLD_ASSET_IDS[@]}" ]; do
        asset_id=${OLD_ASSET_IDS[$asset_index]}
        check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || return 1
        MUTATION_ASSET_ID=$asset_id
        require_etag "$RELEASE_ETAG" || return 1
        if ! api_request old-asset-delete DELETE "repos/$REPO/releases/$RELEASE_ID/assets/$asset_id" \
            --header "If-Match: $RELEASE_ETAG"; then
            return 1
        fi
        refresh_release_etag || return 1
        transaction_remove_asset "$asset_id"
        asset_index=$((asset_index + 1))
    done
    if [ "${#OLD_ASSET_IDS[@]}" -gt 0 ]; then
        fault_after old-asset-delete || return 1
    fi
}

upload_candidates() {
    local archive asset_metadata asset_id
    for archive in \
        "$ASSETS_DIR"/diskdrift-macos-*.tar.gz \
        "$ASSETS_DIR/diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz" \
        "$ASSETS_DIR/diskdrift-windows-x86_64-pc-windows-msvc.zip" \
        "$WORK_DIR/SHA256SUMS"; do
        [ -f "$archive" ] || return 1
        case "$archive" in
            *.tar.gz) content_type=application/gzip ;;
            *.zip) content_type=application/zip ;;
            *) content_type=text/plain ;;
        esac
        MUTATION_ASSET_NAME=${archive##*/}
        MUTATION_ASSET_BYTES=$archive
        MUTATION_ASSET_METADATA="$WORK_DIR/mutation-asset.json"
        asset_metadata="$MUTATION_ASSET_METADATA"
        jq -n --arg name "$MUTATION_ASSET_NAME" --arg type "$content_type" \
            --arg size "$(wc -c < "$archive" | tr -d '[:space:]')" \
            --arg digest "$(hash_file "$archive")" \
            '{name: $name, label: "", content_type: $type, size: ($size | tonumber), digest: ("sha256:" + $digest)}' > "$asset_metadata"
        check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || return 1
        if ! upload_asset "$archive" "${archive##*/}" "$content_type"; then
            return 1
        fi
        asset_id=$(jq -r '.id // empty' "$API_BODY")
        [ -n "$asset_id" ] || return 1
        transaction_add_asset "$API_BODY" "$MUTATION_ASSET_BYTES"
    done
    fault_after candidate-upload || return 1
}

rollback_tag() {
    local payload current_sha current_type status current_exists=0
    if [ "$TAG_MUTATION_PENDING" -eq 0 ]; then
        return 0
    fi
    if ! fault_after tag-rollback; then
        return 1
    fi

    if api_request tag-rollback-inventory GET "repos/$REPO/git/ref/tags/$CHANNEL"; then
        current_exists=1
        TAG_ETAG=$API_ETAG
        current_sha=$(jq -r '.object.sha // empty' "$API_BODY")
        current_type=$(jq -r '.object.type // empty' "$API_BODY")
    else
        status=$?
        [ "$status" -eq 44 ] || return 1
        TAG_ETAG=
    fi

    if [ "$TAG_EXISTS" -eq 1 ] && [ "$current_exists" -eq 1 ] && \
        [ "$current_sha" = "$TAG_SHA" ] && [ "$current_type" = "$TAG_TYPE" ]; then
        TAG_MUTATION_PENDING=0
        return 0
    fi
    if [ "$TAG_EXISTS" -eq 0 ] && [ "$current_exists" -eq 0 ]; then
        TAG_MUTATION_PENDING=0
        return 0
    fi
    if [ "$TAG_EXISTS" -eq 1 ] && [ "$current_exists" -eq 0 ]; then
        ROLLBACK_CONFLICT=1
        log 'tag rollback refused to recreate a baseline tag deleted by another actor'
        return 1
    fi
    if [ "$current_exists" -eq 1 ] && [ "$current_sha" != "$CANDIDATE_SHA" ]; then
        ROLLBACK_CONFLICT=1
        log 'tag rollback refused to overwrite a concurrent tag change'
        return 1
    fi

    if [ "$TAG_EXISTS" -eq 1 ]; then
        require_etag "$TAG_ETAG" || return 1
        payload="$WORK_DIR/tag-rollback.json"
        jq -n --arg sha "$TAG_SHA" '{sha: $sha, force: true}' > "$payload"
        api_request tag-rollback-update PATCH "repos/$REPO/git/refs/tags/$CHANNEL" \
            --header "If-Match: $TAG_ETAG" --input "$payload"
    else
        require_etag "$TAG_ETAG" || return 1
        api_request tag-rollback-delete DELETE "repos/$REPO/git/refs/tags/$CHANNEL" \
            --header "If-Match: $TAG_ETAG"
    fi
    status=$?
    [ "$status" -eq 0 ] || return "$status"
    TAG_MUTATION_PENDING=0
    return 0
}

current_asset_is_owned() {
    local asset_file=$1
    local asset_id=$2
    local expected_file=$3

    jq -S -e --arg id "$asset_id" --slurpfile expected "$expected_file" '
        def contract: {
            name,
            label: (if has("label") then .label else null end),
            content_type: (.content_type // "application/octet-stream"),
            size,
            digest: (.digest // null),
            bytes_sha256: (.bytes_sha256 // null)
        };
        (map(select((.id | tostring) == $id)) | .[0] | contract) as $actual |
        any($expected[0][]; contract == $actual)
    ' "$asset_file" >/dev/null
}

expected_asset_is_present() {
    local asset_file=$1
    local expected_id=$2
    local expected_file=$3

    jq -S -e --arg id "$expected_id" --slurpfile expected "$expected_file" '
        def contract: {
            name,
            label: (if has("label") then .label else null end),
            content_type: (.content_type // "application/octet-stream"),
            size,
            digest: (.digest // null),
            bytes_sha256: (.bytes_sha256 // null)
        };
        ($expected[0] | map(select((.id | tostring) == $id)) | .[0] | contract) as $wanted |
        any(.[]; contract == $wanted)
    ' "$asset_file" >/dev/null
}

rollback_assets_are_owned() {
    local assets_file=$1
    local asset_count asset_index asset_id

    asset_count=$(jq 'length' "$assets_file")
    asset_index=0
    while [ "$asset_index" -lt "$asset_count" ]; do
        asset_id=$(jq -r ".[$asset_index].id | tostring" "$assets_file")
        if ! current_asset_is_owned "$assets_file" "$asset_id" "$BASELINE_ASSETS_FILE" && \
            ! current_asset_is_owned "$assets_file" "$asset_id" "$TRANSACTION_ASSETS_FILE"; then
            ROLLBACK_CONFLICT=1
            log "release rollback refused to overwrite asset $asset_id changed by another actor"
            return 1
        fi
        asset_index=$((asset_index + 1))
    done
}

rollback_release() {
    local current_count asset_index asset_id payload metadata old_name old_label restore_url restored_bytes
    local current_release current_assets current_id
    local -a upload_args=()
    if [ "$RELEASE_TOUCHED" -eq 0 ]; then
        return 0
    fi
    if ! fault_after release-rollback; then
        return 1
    fi

    current_release="$WORK_DIR/current-release.json"
    current_assets="$WORK_DIR/current-assets.json"
    if ! check_expected_release_state "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE"; then
        return 1
    fi

    if [ "$RELEASE_EXISTS" -eq 0 ]; then
        if [ ! -s "$current_release" ] || [ "$(<"$current_release")" = null ]; then
            return 0
        fi
        if ! cmp -s "$current_release" "$TRANSACTION_RELEASE_FILE"; then
            ROLLBACK_CONFLICT=1
            log 'release rollback refused to delete a release changed by another actor'
            return 1
        fi
        if ! rollback_assets_are_owned "$current_assets"; then
            return 1
        fi
        require_etag "$RELEASE_ETAG" || return 1
        api_request release-rollback DELETE "repos/$REPO/releases/$RELEASE_ID" \
            --header "If-Match: $RELEASE_ETAG"
        return $?
    fi

    current_id=$(jq -r '.release_id // empty' "$current_release")
    [ "$current_id" = "$RELEASE_ID" ] || {
        ROLLBACK_CONFLICT=1
        log 'release rollback refused to overwrite a replacement release'
        return 1
    }
    if cmp -s "$current_release" "$BASELINE_RELEASE_FILE" && \
        asset_contracts "$current_assets" > "$WORK_DIR/current-baseline-assets-contract.json" && \
        asset_contracts "$BASELINE_ASSETS_FILE" > "$WORK_DIR/baseline-assets-contract.json" && \
        cmp -s "$WORK_DIR/current-baseline-assets-contract.json" "$WORK_DIR/baseline-assets-contract.json"; then
        return 0
    fi
    rollback_assets_are_owned "$current_assets" || return 1

    cp -- "$current_assets" "$WORK_DIR/rollback-assets-to-delete.json"
    current_count=$(jq 'length' "$WORK_DIR/rollback-assets-to-delete.json")
    asset_index=0
    while [ "$asset_index" -lt "$current_count" ]; do
        asset_id=$(jq -r ".[$asset_index].id | tostring" "$WORK_DIR/rollback-assets-to-delete.json")
        if ! check_expected_release_state "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE"; then
            return 1
        fi
        rollback_assets_are_owned "$current_assets" || return 1
        if current_asset_is_owned "$current_assets" "$asset_id" "$BASELINE_ASSETS_FILE"; then
            asset_index=$((asset_index + 1))
            continue
        fi
        if ! jq -e --arg id "$asset_id" 'any(.[]; ((.id | tostring) == $id))' "$current_assets" >/dev/null; then
            asset_index=$((asset_index + 1))
            continue
        fi
        MUTATION_ASSET_ID=$asset_id
        require_etag "$RELEASE_ETAG" || return 1
        if ! api_request release-rollback-asset-delete DELETE \
            "repos/$REPO/releases/$RELEASE_ID/assets/$asset_id" \
            --header "If-Match: $RELEASE_ETAG"; then
            return 1
        fi
        transaction_remove_asset "$asset_id"
        refresh_release_etag || return 1
        asset_index=$((asset_index + 1))
    done

    if ! check_expected_release_state "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE"; then
        return 1
    fi
    jq 'del(.release_id)' "$BASELINE_RELEASE_FILE" > "$WORK_DIR/release-rollback-expected.json"
    MUTATION_RELEASE_FILE="$WORK_DIR/release-rollback-expected.json"
    require_etag "$RELEASE_ETAG" || return 1
    if ! api_request release-rollback-update PATCH "repos/$REPO/releases/$RELEASE_ID" \
        --header "If-Match: $RELEASE_ETAG" \
        --input "$RELEASE_FIELDS"; then
        return 1
    fi
    RELEASE_ETAG=$API_ETAG
    cp -- "$BASELINE_RELEASE_FILE" "$TRANSACTION_RELEASE_FILE"

    if ! check_expected_release_state "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE"; then
        return 1
    fi
    asset_index=0
    while [ "$asset_index" -lt "${#OLD_ASSET_METADATA[@]}" ]; do
        if ! check_expected_release_state "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE"; then
            return 1
        fi
        rollback_assets_are_owned "$current_assets" || return 1
        metadata=${OLD_ASSET_METADATA[$asset_index]}
        payload="$BACKUP_DIR/assets/$asset_index.bytes"
        old_name=$(jq -r '.name' "$metadata")
        asset_id=$(jq -r '.id | tostring' "$metadata")
        if expected_asset_is_present "$current_assets" "$asset_id" "$BASELINE_ASSETS_FILE"; then
            asset_index=$((asset_index + 1))
            continue
        fi
        MUTATION_ASSET_NAME=$old_name
        MUTATION_ASSET_BYTES=$payload
        MUTATION_ASSET_METADATA=$metadata
        MUTATION_ASSET_ID=
        require_etag "$RELEASE_ETAG" || return 1
        upload_args=(
            --header "If-Match: $RELEASE_ETAG"
            --header "Content-Type: $(jq -r '.content_type // "application/octet-stream"' "$metadata")"
            --raw-field "name=$old_name"
        )
        if jq -e 'has("label") and .label != null' "$metadata" >/dev/null; then
            old_label=$(jq -r '.label' "$metadata")
            upload_args+=(--raw-field "label=$old_label")
        fi
        upload_args+=(--input "$payload")
        if ! api_request release-rollback-asset-upload POST \
            "$RELEASE_UPLOAD_URL" "${upload_args[@]}"; then
            return 1
        fi
        if ! verify_restored_asset "$metadata" "$payload" "$API_BODY"; then
            log "restored release asset metadata or bytes did not match ${old_name}"
            return 1
        fi
        restore_url=$(jq -r '.url // .browser_download_url // empty' "$API_BODY")
        [ -n "$restore_url" ] || return 1
        restored_bytes="$WORK_DIR/restored-asset-$asset_index.bytes"
        if ! download_asset release-rollback-asset-verify "$restore_url" "$restored_bytes"; then
            return 1
        fi
        if ! verify_asset_backup "$metadata" "$restored_bytes"; then
            log "restored release asset bytes did not match ${old_name}"
            return 1
        fi
        asset_id=$(jq -r '.id // empty' "$API_BODY")
        [ -n "$asset_id" ] || return 1
        transaction_add_asset "$API_BODY" "$MUTATION_ASSET_BYTES"
        refresh_release_etag || return 1
        asset_index=$((asset_index + 1))
    done
    if ! state_matches "$BASELINE_TAG_FILE" "$BASELINE_RELEASE_FILE" "$BASELINE_ASSETS_FILE"; then
        ROLLBACK_CONFLICT=1
        log 'release rollback did not restore the baseline without concurrent changes'
        return 1
    fi
    return 0
}

rollback() {
    local tag_status=0 release_status=0
    if rollback_tag; then
        log 'tag rollback: restored'
    else
        tag_status=1
        log 'tag rollback: FAILED'
    fi
    if rollback_release; then
        log 'release rollback: restored'
    else
        release_status=1
        log 'release rollback: FAILED'
    fi
    if [ "$tag_status" -ne 0 ] || [ "$release_status" -ne 0 ]; then
        return 1
    fi
    return 0
}

parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --repo)
                [ "$#" -ge 2 ] || usage
                REPO=$2
                shift 2
                ;;
            --sha)
                [ "$#" -ge 2 ] || usage
                CANDIDATE_SHA=$2
                shift 2
                ;;
            --assets-dir)
                [ "$#" -ge 2 ] || usage
                ASSETS_DIR=$2
                shift 2
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            --fault-after)
                [ "$#" -ge 2 ] || usage
                FAULT_AFTER=$2
                shift 2
                ;;
            --help|-h)
                usage
                ;;
            *)
                usage
                ;;
        esac
    done

    [[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || usage
    [[ "$CANDIDATE_SHA" =~ ^[0-9a-fA-F]{40}$ ]] || usage
    [ -n "$ASSETS_DIR" ] || usage
    if [ -n "$FAULT_AFTER" ] && ! has_stage "$FAULT_AFTER"; then
        usage
    fi
    CANDIDATE_SHA=$(printf '%s' "$CANDIDATE_SHA" | tr 'A-F' 'a-f')
}

main() {
    parse_args "$@"
    if [ "$DRY_RUN" -eq 0 ] && [ -z "${GH_TOKEN:-}" ]; then
        usage
    fi
    for tool in gh jq tar unzip zipinfo git awk cut tr wc find; do
        command -v "$tool" >/dev/null 2>&1 || usage
    done
    hash_available || usage

    WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/publish-unstable.XXXXXX")
    BACKUP_DIR="$WORK_DIR/backup"
    mkdir -p "$BACKUP_DIR"

    validate_candidate
    run_public_tree_check
    if ! inventory; then
        publication_error 'could not inventory the unstable tag or release'
    fi
    if ! check_candidate_remote candidate; then
        candidate_error 'candidate is not the current merged devel commit'
    fi

    if [ "$DRY_RUN" -eq 1 ]; then
        log 'dry-run: candidate and unstable inventory validated; no mutation performed'
        return 0
    fi

    record_transaction_state "$BASELINE_TAG_FILE" "$BASELINE_RELEASE_FILE" "$BASELINE_ASSETS_FILE"
    check_expected_state "$BASELINE_TAG_FILE" "$BASELINE_RELEASE_FILE" "$BASELINE_ASSETS_FILE" || \
        publication_error 'concurrent mutation detected before tag mutation'
    fault_after before-tag-mutation || publication_error 'fault injected before tag mutation'
    MUTATION_STARTED=1
    # Keep the final live check adjacent to the first tag mutation.
    if ! check_candidate_remote final; then
        candidate_error 'candidate is not the current merged devel commit immediately before tag mutation'
    fi
    if ! mutate_tag; then
        publication_error 'tag mutation failed'
    fi
    jq -S '{object: .object}' "$API_BODY" > "$TRANSACTION_TAG_FILE"
    check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || \
        publication_error 'concurrent mutation detected after tag mutation'
    if ! check_candidate_remote release; then
        publication_error 'candidate changed before release mutation'
    fi
    check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || \
        publication_error 'concurrent mutation detected before release mutation'
    fault_after before-release-mutation || publication_error 'fault injected before release mutation'
    if ! mutate_release; then
        publication_error 'release mutation failed'
    fi
    check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || \
        publication_error 'concurrent mutation detected after release mutation'
    if ! delete_old_assets; then
        publication_error 'old asset deletion failed'
    fi
    if ! upload_candidates; then
        publication_error 'candidate asset upload failed'
    fi
    check_expected_state "$TRANSACTION_TAG_FILE" "$TRANSACTION_RELEASE_FILE" "$TRANSACTION_ASSETS_FILE" || \
        publication_error 'concurrent mutation detected after candidate upload'
    MUTATION_STARTED=0
    # The fixture uses this hold to exercise signal handling after publication.
    if [ -n "${PUBLISH_TEST_COMPLETION_FILE:-}" ]; then
        : > "$PUBLISH_TEST_COMPLETION_FILE"
        while [ ! -f "$PUBLISH_TEST_COMPLETION_FILE.continue" ]; do
            sleep 0.01
        done
    fi
    log "published $CHANNEL for devel commit $CANDIDATE_SHA"
}

main "$@"
