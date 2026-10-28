#!/usr/bin/env bash

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
SCRIPT="$ROOT/scripts/publish-unstable.sh"
mkdir -p "$ROOT/target"
FIXTURE=$(mktemp -d "$ROOT/target/publish-unstable-test.XXXXXX")
FAKE_BIN="$FIXTURE/bin"
STATE="$FIXTURE/state"
CANDIDATE="$FIXTURE/candidate"

cleanup() {
    rm -rf "$FIXTURE"
}
trap cleanup EXIT
trap 'exit 129' HUP INT TERM

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_eq() {
    [ "$1" = "$2" ] || fail "expected '$2', got '$1'"
}

assert_status() {
    expected=$1
    shift
    set +e
    "$@"
    actual=$?
    set -e
    [ "$actual" -eq "$expected" ] || fail "expected exit $expected, got $actual: $*"
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

mkdir -p "$FAKE_BIN" "$STATE" "$CANDIDATE"
RESPONSE_DIR="$FIXTURE/responses"
mkdir -p "$RESPONSE_DIR"
export TMPDIR="$FIXTURE/tmp"
mkdir -p "$TMPDIR"

if [ "${PUBLISH_TEST_STOCK_BASH:-0}" = 1 ]; then
    ln -s /bin/bash "$FAKE_BIN/bash"
    PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin"
    export PATH
    case "$BASH_VERSION" in
        3.2*) ;;
        *) fail "stock Bash test is running Bash $BASH_VERSION" ;;
    esac
    command -v sha256sum >/dev/null 2>&1 && fail 'stock Bash test unexpectedly found sha256sum'
    command -v shasum >/dev/null 2>&1 || fail 'stock Bash test could not find shasum'
fi

# The fixture is deliberately a standalone public checkout: the publisher must
# run the real public-tree check without consulting the private worktree.
mkdir -p "$FIXTURE/public-checkout/scripts"
git -C "$FIXTURE/public-checkout" init -q
git -C "$FIXTURE/public-checkout" config user.name Test
git -C "$FIXTURE/public-checkout" config user.email test@example.invalid
printf '%s\n' '# public' > "$FIXTURE/public-checkout/README.md"
git -C "$FIXTURE/public-checkout" add README.md
git -C "$FIXTURE/public-checkout" commit -q -m public
PUBLIC_CHECK="$FIXTURE/public-checkout"

cat > "$FAKE_BIN/gh" <<'FAKE_GH'
#!/usr/bin/env bash
set -euo pipefail

STATE=${FAKE_GH_STATE:?}
REPO=${FAKE_GH_REPO:?}
RESPONSE_DIR=${FAKE_GH_RESPONSE_DIR:?}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

method=GET
input=
endpoint=
content_type=application/octet-stream
name=
label=
if_match=
if_none_match=
include=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        api) shift ;;
        --method|-X) method=$2; shift 2 ;;
        --input) input=$2; shift 2 ;;
        --field)
            printf '%s\n' 'fake gh rejected typed --field; publisher must use --raw-field' >&2
            exit 2
            ;;
        --raw-field)
            case "$2" in
                name=*) name=${2#name=} ;;
                label=*) label=${2#label=} ;;
            esac
            shift 2
            ;;
        --header)
            case "$2" in
                Content-Type:*) content_type=${2#Content-Type: } ;;
                If-Match:*) if_match=${2#If-Match: } ;;
                If-None-Match:*) if_none_match=${2#If-None-Match: } ;;
            esac
            shift 2
            ;;
        --include) include=1; shift ;;
        --silent) shift ;;
        --jq) shift 2 ;;
        --*) shift ;;
        *) endpoint=$1; shift; break ;;
    esac
done

[ -n "$endpoint" ] || { printf '%s\n' 'fake gh: missing endpoint' >&2; exit 1; }

emit() {
    status=$1
    body=$2
    etag=${3:-}
    mkdir -p "$RESPONSE_DIR"
    if [ "$include" -eq 1 ]; then
        printf 'HTTP/1.1 %s Fixture\r\n' "$status"
        [ -z "$etag" ] || printf 'ETag: %s\r\n' "$etag"
        printf '\r\n'
    fi
    [ -z "$body" ] || printf '%s\n' "$body"
    [ "$status" -lt 400 ]
}

etag_for_file() {
    printf '"%s"' "$(hash_file "$1")"
}

precondition_failure() {
    printf '%s\n' 'precondition failure' > "$STATE/precondition-failure"
    emit 428 '{"message":"precondition required"}' || true
    exit 1
}

check_precondition() {
    target=$1
    [ "${FAKE_GH_REQUIRE_ETAG:-0}" = 1 ] || return 0
    if [ -n "$if_match" ]; then
        [ -f "$target" ] || precondition_failure
        [ "$if_match" = "$(etag_for_file "$target")" ] || {
            printf '%s\n' 'etag mismatch' > "$STATE/precondition-failure"
            emit 412 '{"message":"etag mismatch"}' || true
            exit 1
        }
        return 0
    fi
    [ "$if_none_match" = '*' ] || precondition_failure
    [ ! -e "$target" ] || {
        printf '%s\n' 'resource already exists' > "$STATE/precondition-failure"
        emit 412 '{"message":"resource already exists"}' || true
        exit 1
    }
}

race_before_mutation() {
    [ "${FAKE_GH_RACE_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 0
    [ ! -f "$STATE/race-mutated" ] || return 0
    case "$PUBLISH_OPERATION" in
        tag-create|tag-update|tag-rollback-update)
            printf '%s\n' '{"ref":"refs/tags/unstable","object":{"type":"commit","sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}' > "$STATE/tag.json"
            ;;
        *)
            [ -f "$STATE/release.json" ] || return 0
            jq '.name = "actor release"' "$STATE/release.json" > "$RESPONSE_DIR/race-release.json"
            mv "$RESPONSE_DIR/race-release.json" "$STATE/release.json"
            ;;
    esac
    touch "$STATE/race-mutated"
}

signal_parent_once() {
    [ "${FAKE_GH_SIGNAL_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 0
    [ ! -f "$STATE/signal-sent" ] || return 0
    touch "$STATE/signal-sent"
    kill -TERM "$PPID"
}

fail_operation() {
    [ "${FAKE_GH_FAIL_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 1
    printf 'injected fake-gh failure at %s\n' "$PUBLISH_OPERATION" >&2
    emit 500 '{"message":"injected failure"}' || true
    exit 1
}
fail_operation || true

response_failure() {
    [ "${FAKE_GH_RESPONSE_FAILURE_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 1
    emit 500 '{"message":"response delivery failure"}' || true
    exit 1
}

malformed_once() {
    [ "${FAKE_GH_MALFORMED_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 1
    count=0
    [ ! -f "$STATE/malformed-$PUBLISH_OPERATION.count" ] || count=$(<"$STATE/malformed-$PUBLISH_OPERATION.count")
    [ "$count" -eq 0 ] || return 1
    printf '1\n' > "$STATE/malformed-$PUBLISH_OPERATION.count"
    emit 200 'not-json'
    exit 0
}
malformed_once || true

concurrent_mutation_once() {
    [ "${FAKE_GH_CONCURRENT_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 1
    [ ! -f "$STATE/concurrent-mutated" ] || return 1
    count=0
    [ ! -f "$STATE/concurrent-count" ] || count=$(<"$STATE/concurrent-count")
    count=$((count + 1))
    printf '%s\n' "$count" > "$STATE/concurrent-count"
    [ "$count" -ge "${FAKE_GH_CONCURRENT_AFTER:-1}" ] || return 1
    if [ "${PUBLISH_OPERATION:-}" = tag-rollback-inventory ]; then
        rm -f "$STATE/tag.json"
    elif [ "${PUBLISH_OPERATION:-}" = rollback-inventory-assets ]; then
        rm -f "$STATE/assets/legacy.bytes" "$STATE/assets/legacy.json"
        jq ' .assets |= map(select((.name // "") != "legacy.txt") )' "$STATE/release.json" > "$RESPONSE_DIR/concurrent-release.json"
        mv "$RESPONSE_DIR/concurrent-release.json" "$STATE/release.json"
    elif [ "${PUBLISH_OPERATION:-}" = state-inventory-tag ]; then
        printf '%s\n' '{"ref":"refs/tags/unstable","object":{"type":"commit","sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}' > "$STATE/tag.json"
    elif [ "${PUBLISH_OPERATION:-}" = state-inventory-assets ]; then
        [ -f "$STATE/release.json" ] || return 0
        printf 'actor bytes\n' > "$STATE/assets/actor.bytes"
        actor_size=$(wc -c < "$STATE/assets/actor.bytes" | tr -d '[:space:]')
        actor_digest=$(hash_file "$STATE/assets/actor.bytes")
        printf '{"id":"actor","name":"actor.txt","label":"actor","content_type":"text/plain","state":"uploaded","size":%s,"digest":"sha256:%s","url":"https://uploads.example/assets/actor"}\n' \
            "$actor_size" "$actor_digest" > "$STATE/assets/actor.json"
        jq --slurpfile actor "$STATE/assets/actor.json" '.name = "actor release" | .assets += [$actor[0]]' \
            "$STATE/release.json" > "$RESPONSE_DIR/concurrent-release.json"
    else
        [ -f "$STATE/release.json" ] || return 0
        jq '.name = "actor release"' "$STATE/release.json" > "$RESPONSE_DIR/concurrent-release.json"
    fi
    mv "$RESPONSE_DIR/concurrent-release.json" "$STATE/release.json"
    touch "$STATE/concurrent-mutated"
}

concurrent_mutation_once || true

mutate_devel_once() {
    [ "${FAKE_GH_MUTATE_DEVEL_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ] || return 1
    [ ! -f "$STATE/devel-mutated" ] || return 1
    [ -n "${FAKE_GH_MUTATE_DEVEL_SHA:-}" ] || return 1
    printf '%s\n' "$FAKE_GH_MUTATE_DEVEL_SHA" > "$STATE/devel.sha"
    touch "$STATE/devel-mutated"
}

mutate_devel_once || true

case "$method $endpoint" in
    "GET repos/$REPO/git/ref/tags/unstable")
        signal_parent_once
        if [ -f "$STATE/tag.json" ]; then
            emit 200 "$(<"$STATE/tag.json")" "$(etag_for_file "$STATE/tag.json")"
        else
            emit 404 '{"message":"Not Found"}' || true
            exit 1
        fi
        ;;
    "GET repos/$REPO/git/ref/heads/devel")
        sha=$(<"$STATE/devel.sha")
        [ "${PUBLISH_OPERATION:-}" = final-devel ] && [ -n "${FAKE_GH_FINAL_DEVEL_SHA:-}" ] && sha=$FAKE_GH_FINAL_DEVEL_SHA
        emit 200 "{\"object\":{\"type\":\"commit\",\"sha\":\"$sha\"}}"
        ;;
    "GET repos/$REPO/releases/tags/unstable")
        if [ -f "$STATE/release.json" ]; then
            emit 200 "$(<"$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
        else
            emit 404 '{"message":"Not Found"}' || true
            exit 1
        fi
        ;;
    "GET repos/$REPO/commits/"*"/pulls?base=devel")
        emit 200 '[{"base":{"ref":"devel"},"merged_at":"2026-10-28T00:00:00Z"}]'
        ;;
    "GET repos/$REPO/releases/"*"/assets?per_page=100")
        if [ -f "$STATE/release.json" ]; then
            if [ "${FAKE_GH_PAGINATED_ASSETS:-0}" = 1 ]; then
                emit 200 "$(jq -c '.assets as $assets | if ($assets | length) > 1 then [[$assets[0]], $assets[1:]] else [$assets] end' "$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
            else
                emit 200 "$(jq -c '[.assets]' "$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
            fi
        else
            emit 404 '{"message":"Not Found"}' || true
            exit 1
        fi
        ;;
    "GET repos/$REPO/releases/"*)
        if [ -f "$STATE/release.json" ]; then
            emit 200 "$(<"$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
        else
            emit 404 '{"message":"Not Found"}' || true
            exit 1
        fi
        ;;
    "POST repos/$REPO/git/refs")
        race_before_mutation
        check_precondition "$STATE/tag.json"
        sha=$(jq -r '.sha' "$input")
        printf '{"ref":"refs/tags/unstable","object":{"type":"commit","sha":"%s"}}\n' "$sha" > "$STATE/tag.json"
        signal_parent_once
        response_failure || true
        emit 201 "$(<"$STATE/tag.json")" "$(etag_for_file "$STATE/tag.json")"
        ;;
    "PATCH repos/$REPO/git/refs/tags/unstable")
        race_before_mutation
        check_precondition "$STATE/tag.json"
        sha=$(jq -r '.sha' "$input")
        type=commit
        [ "$sha" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ] && type=tag
        printf '{"ref":"refs/tags/unstable","object":{"type":"%s","sha":"%s"}}\n' "$type" "$sha" > "$STATE/tag.json"
        signal_parent_once
        response_failure || true
        emit 200 "$(<"$STATE/tag.json")" "$(etag_for_file "$STATE/tag.json")"
        ;;
    "DELETE repos/$REPO/git/ref/tags/unstable"|"DELETE repos/$REPO/git/refs/tags/unstable")
        race_before_mutation
        check_precondition "$STATE/tag.json"
        rm -f "$STATE/tag.json"
        emit 204 ''
        ;;
    "POST repos/$REPO/releases")
        race_before_mutation
        check_precondition "$STATE/release.json"
        jq --argjson id 9001 --arg upload_url 'https://uploads.example/release/9001/assets{?name,label}' \
            '. + {id: $id, upload_url: $upload_url, assets: []}' "$input" > "$STATE/release.json"
        response_failure || true
        emit 201 "$(<"$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
        ;;
    "PATCH repos/$REPO/releases/"*)
        race_before_mutation
        check_precondition "$STATE/release.json"
        if [ "${PUBLISH_OPERATION:-}" = release-rollback-update ]; then
            jq -s '.[0] * .[1] | del(.generate_release_notes)' "$STATE/release.json" "$input" > "$RESPONSE_DIR/release.json"
        else
            jq -s '.[0] * .[1]' "$STATE/release.json" "$input" > "$RESPONSE_DIR/release.json"
        fi
        mv "$RESPONSE_DIR/release.json" "$STATE/release.json"
        response_failure || true
        emit 200 "$(<"$STATE/release.json")" "$(etag_for_file "$STATE/release.json")"
        ;;
    "DELETE repos/$REPO/releases/"*"/assets/"*)
        asset_id=${endpoint##*/}
        race_before_mutation
        check_precondition "$STATE/release.json"
        rm -f "$STATE/assets/$asset_id.bytes" "$STATE/assets/$asset_id.json"
        jq --arg id "$asset_id" '.assets |= map(select((.id | tostring) != $id))' "$STATE/release.json" > "$RESPONSE_DIR/release.json"
        mv "$RESPONSE_DIR/release.json" "$STATE/release.json"
        if [ "${FAKE_GH_DELETE_404_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ]; then
            emit 404 '{"message":"Not Found"}' || true
            exit 1
        fi
        response_failure || true
        emit 204 ''
        ;;
    "DELETE repos/$REPO/releases/"*)
        race_before_mutation
        check_precondition "$STATE/release.json"
        rm -f "$STATE/release.json"
        rm -rf "$STATE/assets"
        response_failure || true
        emit 204 ''
        ;;
    "POST https://uploads.example/release/9001/assets")
        race_before_mutation
        check_precondition "$STATE/release.json"
        mkdir -p "$STATE/assets"
        counter=0
        [ ! -f "$STATE/asset-counter" ] || counter=$(<"$STATE/asset-counter")
        counter=$((counter + 1))
        printf '%s\n' "$counter" > "$STATE/asset-counter"
        asset_id=asset-$counter
        cp "$input" "$STATE/assets/$asset_id.bytes"
        size=$(wc -c < "$input" | tr -d '[:space:]')
        digest=$(hash_file "$input")
        if [ "${FAKE_GH_NULL_DIGEST:-0}" = 1 ]; then
            printf '{"id":"%s","name":"%s","label":"%s","content_type":"%s","state":"uploaded","size":%s,"digest":null,"url":"https://uploads.example/assets/%s"}\n' \
                "$asset_id" "$name" "$label" "$content_type" "$size" "$asset_id" > "$STATE/assets/$asset_id.json"
        else
            printf '{"id":"%s","name":"%s","label":"%s","content_type":"%s","state":"uploaded","size":%s,"digest":"sha256:%s","url":"https://uploads.example/assets/%s"}\n' \
                "$asset_id" "$name" "$label" "$content_type" "$size" "$digest" "$asset_id" > "$STATE/assets/$asset_id.json"
        fi
        jq --slurpfile asset "$STATE/assets/$asset_id.json" '.assets += [$asset[0]]' "$STATE/release.json" > "$RESPONSE_DIR/release.json"
        mv "$RESPONSE_DIR/release.json" "$STATE/release.json"
        response_failure || true
        emit 201 "$(<"$STATE/assets/$asset_id.json")"
        ;;
    "GET https://uploads.example/"*)
        asset_id=${endpoint##*/}
        if [ "${FAKE_GH_CORRUPT_DOWNLOAD_OPERATION:-}" = "${PUBLISH_OPERATION:-}" ]; then
            printf 'corrupt bytes\n'
        else
            cat "$STATE/assets/$asset_id.bytes"
        fi
        ;;
    "POST repos/$REPO/releases/"*"/assets")
        printf '%s\n' 'fake gh rejected the default REST asset upload endpoint' >&2
        exit 1
        ;;
    *)
        printf 'fake gh: unsupported %s %s\n' "$method" "$endpoint" >&2
        exit 1
        ;;
esac
FAKE_GH
chmod +x "$FAKE_BIN/gh"

make_candidate() {
    rm -rf "$CANDIDATE"
    mkdir -p "$CANDIDATE"
    for runner in macos linux windows; do
        case "$runner" in
            macos) target=aarch64-apple-darwin; suffix=tar.gz; binary=diskdrift-macos-$target; archive=diskdrift-macos-$target.tar.gz ;;
            linux) target=x86_64-unknown-linux-gnu; suffix=tar.gz; binary=diskdrift-linux-$target; archive=diskdrift-linux-$target.tar.gz ;;
            windows) target=x86_64-pc-windows-msvc; suffix=zip; binary=diskdrift-windows-$target.exe; archive=diskdrift-windows-$target.zip ;;
        esac
        work="$FIXTURE/$runner"
        rm -rf "$work"
        mkdir -p "$work"
        printf '%s binary\n' "$runner" > "$work/$binary"
        if [ "$suffix" = tar.gz ]; then
            tar -czf "$CANDIDATE/$archive" -C "$work" "$binary"
        else
            (cd "$work" && zip -q "$CANDIDATE/$archive" "$binary")
        fi
        digest=$(hash_file "$CANDIDATE/$archive")
        printf '{"schema":1,"source_sha":"%s","runner":"%s","rust_target":"%s","binary":"%s","sha256":"%s"}\n' \
            "$CANDIDATE_SHA" "$runner" "$target" "$binary" "$digest" > "$CANDIDATE/manifest-$runner.json"
    done
}

make_release_state() {
    mkdir -p "$STATE/assets"
    printf 'legacy bytes\n' > "$STATE/assets/legacy.bytes"
    legacy_size=$(wc -c < "$STATE/assets/legacy.bytes" | tr -d '[:space:]')
    legacy_digest=$(hash_file "$STATE/assets/legacy.bytes")
    legacy_digest_field='"sha256:'"$legacy_digest"'"'
    [ "${FAKE_GH_NULL_DIGEST:-0}" = 1 ] && legacy_digest_field=null
    printf '{"id":"legacy","name":"legacy.txt","label":"true","content_type":"text/plain","state":"uploaded","size":%s,"digest":%s,"url":"https://uploads.example/assets/legacy"}\n' \
        "$legacy_size" "$legacy_digest_field" > "$STATE/assets/legacy.json"
    release_assets="$(<"$STATE/assets/legacy.json")"
    if [ "${FAKE_GH_PAGINATED_ASSETS:-0}" = 1 ]; then
        printf 'secondary bytes\n' > "$STATE/assets/secondary.bytes"
        secondary_size=$(wc -c < "$STATE/assets/secondary.bytes" | tr -d '[:space:]')
        secondary_digest=$(hash_file "$STATE/assets/secondary.bytes")
        secondary_digest_field='"sha256:'"$secondary_digest"'"'
        [ "${FAKE_GH_NULL_DIGEST:-0}" = 1 ] && secondary_digest_field=null
        printf '{"id":"secondary","name":"secondary.txt","label":"page-2","content_type":"text/plain","state":"uploaded","size":%s,"digest":%s,"url":"https://uploads.example/assets/secondary"}\n' \
            "$secondary_size" "$secondary_digest_field" > "$STATE/assets/secondary.json"
        release_assets="$release_assets,$(<"$STATE/assets/secondary.json")"
    fi
    printf '{"id":9001,"upload_url":"https://uploads.example/release/9001/assets{?name,label}","tag_name":"unstable","name":"Old title","body":"Old body","draft":true,"prerelease":false,"target_commitish":"old","assets":[%s]}\n' \
        "$release_assets" > "$STATE/release.json"
}

make_neither_state() {
    rm -rf "$STATE"
    mkdir -p "$STATE"
    printf '%s\n' "$CANDIDATE_SHA" > "$STATE/devel.sha"
    make_candidate
}

snapshot_state() {
    snapshot=$1
    rm -rf "$snapshot"
    mkdir -p "$snapshot"
    [ ! -f "$STATE/tag.json" ] || jq -c . "$STATE/tag.json" > "$snapshot/tag.json"
    [ ! -f "$STATE/release.json" ] || jq -c . "$STATE/release.json" > "$snapshot/release.json"
    if [ -d "$STATE/assets" ]; then
        mkdir -p "$snapshot/assets"
        for metadata in "$STATE"/assets/*.json; do
            [ -f "$metadata" ] || continue
            jq -c . "$metadata" > "$snapshot/assets/${metadata##*/}"
            bytes=${metadata%.json}.bytes
            [ ! -f "$bytes" ] || cp "$bytes" "$snapshot/assets/${bytes##*/}"
        done
    fi
}

assert_state_restored() {
    expected=$1
    actual=$2
    expected_release="$FIXTURE/expected-release.json"
    actual_release="$FIXTURE/actual-release.json"
    expected_contract="$FIXTURE/expected-contract.json"
    actual_contract="$FIXTURE/actual-contract.json"
    expected_bytes="$FIXTURE/expected-bytes.txt"
    actual_bytes="$FIXTURE/actual-bytes.txt"

    if [ -f "$expected/tag.json" ]; then
        [ -f "$actual/tag.json" ] || fail "state was not restored for $3"
        cmp -s "$expected/tag.json" "$actual/tag.json" || fail "state was not restored for $3"
    else
        [ ! -f "$actual/tag.json" ] || fail "state was not restored for $3"
    fi

    if [ -f "$expected/release.json" ]; then
        [ -f "$actual/release.json" ] || fail "state was not restored for $3"
        jq -S 'del(.assets)' "$expected/release.json" > "$expected_release"
        jq -S 'del(.assets)' "$actual/release.json" > "$actual_release"
        cmp -s "$expected_release" "$actual_release" || fail "state was not restored for $3"
    else
        [ ! -f "$actual/release.json" ] || fail "state was not restored for $3"
    fi

    asset_contracts_from_release "$expected/release.json" > "$expected_contract"
    asset_contracts_from_release "$actual/release.json" > "$actual_contract"
    cmp -s "$expected_contract" "$actual_contract" || fail "state was not restored for $3"
    asset_bytes_hashes "$expected" > "$expected_bytes"
    asset_bytes_hashes "$actual" > "$actual_bytes"
    cmp -s "$expected_bytes" "$actual_bytes" || fail "state was not restored for $3"
}

asset_contracts_from_release() {
    local release=$1
    if [ -f "$release" ]; then
        jq -S '(.assets // []) | map({name, label: (if has("label") then .label else null end), content_type: (.content_type // "application/octet-stream"), size, digest: (.digest // null)}) |
            sort_by([.name, (.label | tostring), .content_type, (.size | tostring), (.digest | tostring)])' "$release"
    else
        printf '%s\n' '[]'
    fi
}

asset_bytes_hashes() {
    local directory=$1
    local hashes="$FIXTURE/asset-hashes.$$.txt"
    : > "$hashes"
    if [ -d "$directory/assets" ]; then
        for bytes in "$directory"/assets/*.bytes; do
            [ -f "$bytes" ] || continue
            hash_file "$bytes" >> "$hashes"
        done
    fi
    sort "$hashes"
    rm -f "$hashes"
}

assert_success_state() {
    jq -e --arg sha "$CANDIDATE_SHA" '.object.type == "commit" and .object.sha == $sha' "$STATE/tag.json" >/dev/null \
        || fail 'successful publication did not move the unstable tag'
    jq -e --arg sha "$CANDIDATE_SHA" '
        .name == "DiskDrift unstable" and
        .body == ("Unstable development release for devel commit " + $sha + ".") and
        .draft == false and .prerelease == true and .generate_release_notes == false and
        (.assets | map(.name) | sort) == [
            "SHA256SUMS",
            "diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz",
            "diskdrift-macos-aarch64-apple-darwin.tar.gz",
            "diskdrift-windows-x86_64-pc-windows-msvc.zip"
        ]
    ' "$STATE/release.json" >/dev/null || fail 'successful publication has the wrong release shape'
    if jq -r '.name, .body' "$STATE/release.json" | grep -E 'Co-Authored-By:|Claude-Session:|Generated with \[Claude Code\]|AI|agent' >/dev/null; then
        fail 'fixed release text contains agent or AI attribution'
    fi
}

export PATH="$FAKE_BIN:$PATH"
export FAKE_GH_STATE="$STATE"
export FAKE_GH_REPO=owner/repo
export FAKE_GH_RESPONSE_DIR="$RESPONSE_DIR"
unset GH_TOKEN
CANDIDATE_SHA=$(git -C "$PUBLIC_CHECK" rev-parse HEAD)
export CANDIDATE_SHA
printf '%s\n' "$CANDIDATE_SHA" > "$STATE/devel.sha"

run_publisher() {
    (cd "$PUBLIC_CHECK" && "$SCRIPT" "$@")
}

run_publisher_capture() {
    local output=$1
    shift
    run_publisher "$@" >"$output" 2>&1
}

make_candidate
assert_status 2 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA"
export GH_TOKEN=not-used
unset GH_TOKEN
assert_status 2 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
export GH_TOKEN=not-used
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$FIXTURE/missing"

for state in neither tag-only release-only both; do
    rm -rf "$STATE" "$FIXTURE/before-$state"
    mkdir -p "$STATE"
    printf '%s\n' "$CANDIDATE_SHA" > "$STATE/devel.sha"
    make_candidate
    case "$state" in
        tag-only|both)
            printf '{"ref":"refs/tags/unstable","object":{"type":"tag","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}\n' > "$STATE/tag.json"
            ;;
    esac
    case "$state" in
        release-only|both)
            make_release_state
            ;;
    esac
    snapshot_state "$FIXTURE/before-$state"
    export GH_TOKEN=fake
    assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
    assert_state_restored "$FIXTURE/before-$state" "$STATE" "$state dry-run"
    assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
    assert_state_restored "$FIXTURE/before-$state" "$STATE" "$state dry-run repeat"
    assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
    assert_success_state
done

make_candidate
printf '%s\n' stale > "$CANDIDATE/stale.txt"
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
make_candidate
ln -s diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz "$CANDIDATE/stale-link"
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run

make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-malformed-pagination"
export FAKE_GH_MALFORMED_OPERATION=inventory-assets
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
unset FAKE_GH_MALFORMED_OPERATION
assert_state_restored "$FIXTURE/before-malformed-pagination" "$STATE" 'malformed pagination retry'

make_neither_state
printf '{"ref":"refs/tags/unstable","object":{"type":"tag","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}\n' > "$STATE/tag.json"
snapshot_state "$FIXTURE/before-malformed-api"
export FAKE_GH_MALFORMED_OPERATION=inventory-tag
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
unset FAKE_GH_MALFORMED_OPERATION
assert_state_restored "$FIXTURE/before-malformed-api" "$STATE" 'malformed API retry'

make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-corrupt-backup"
export FAKE_GH_CORRUPT_DOWNLOAD_OPERATION=backup-asset
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_CORRUPT_DOWNLOAD_OPERATION
assert_state_restored "$FIXTURE/before-corrupt-backup" "$STATE" 'corrupt backup rejection'

make_neither_state
export FAKE_GH_FAIL_OPERATION=final-merged-pr
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_FAIL_OPERATION
[ ! -f "$STATE/tag.json" ] || fail 'final merged-PR failure mutated the tag'

make_neither_state
export FAKE_GH_FINAL_DEVEL_SHA=ffffffffffffffffffffffffffffffffffffffff
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_FINAL_DEVEL_SHA
[ ! -f "$STATE/tag.json" ] || fail 'final devel SHA failure mutated the tag'

make_neither_state
export FAKE_GH_MUTATE_DEVEL_OPERATION=state-inventory-tag
export FAKE_GH_MUTATE_DEVEL_SHA=ffffffffffffffffffffffffffffffffffffffff
assert_status 3 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_MUTATE_DEVEL_OPERATION
unset FAKE_GH_MUTATE_DEVEL_SHA
[ ! -f "$STATE/tag.json" ] || fail 'devel change in the pre-tag check gap mutated the tag'

make_neither_state
export FAKE_GH_RESPONSE_FAILURE_OPERATION=tag-create
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_RESPONSE_FAILURE_OPERATION
assert_success_state

make_neither_state
snapshot_state "$FIXTURE/before-first-mutation-fault"
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after before-tag-mutation
assert_state_restored "$FIXTURE/before-first-mutation-fault" "$STATE" 'fault before first mutation'

make_neither_state
snapshot_state "$FIXTURE/before-second-mutation-fault"
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after before-release-mutation
assert_state_restored "$FIXTURE/before-second-mutation-fault" "$STATE" 'fault between mutations'

for stage in tag-create tag-update release-create release-update old-asset-delete candidate-upload; do
    case "$stage" in
        tag-create) states='neither release-only' ;;
        tag-update) states='tag-only both' ;;
        release-create) states='neither tag-only' ;;
        release-update|old-asset-delete) states='release-only both' ;;
        candidate-upload) states='neither tag-only release-only both' ;;
    esac
    for state in $states; do
        rm -rf "$STATE" "$FIXTURE/before-$stage-$state"
        mkdir -p "$STATE"
        printf '%s\n' "$CANDIDATE_SHA" > "$STATE/devel.sha"
        make_candidate
        case "$state" in
            tag-only|both) printf '{"ref":"refs/tags/unstable","object":{"type":"tag","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}\n' > "$STATE/tag.json" ;;
        esac
        case "$state" in
            release-only|both)
                make_release_state
                ;;
        esac
        snapshot_state "$FIXTURE/before-$stage-$state"
        assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after "$stage"
        assert_state_restored "$FIXTURE/before-$stage-$state" "$STATE" "$stage/$state"
    done
done

for stage in tag-rollback release-rollback; do
    rm -rf "$STATE" "$FIXTURE/before-$stage"
    mkdir -p "$STATE"
    printf '%s\n' "$CANDIDATE_SHA" > "$STATE/devel.sha"
    make_candidate
    snapshot_state "$FIXTURE/before-$stage"
    export FAKE_GH_FAIL_OPERATION=candidate-asset-upload
    assert_status 5 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after "$stage"
    if [ "$stage" = tag-rollback ]; then
        [ ! -f "$STATE/release.json" ] || fail 'release rollback did not run after tag rollback failed'
        jq -e --arg sha "$CANDIDATE_SHA" '.object.sha == $sha' "$STATE/tag.json" >/dev/null \
            || fail 'tag rollback failure did not leave the other mutation visible'
    else
        [ ! -f "$STATE/tag.json" ] || fail 'tag rollback did not run after release rollback failed'
        [ -f "$STATE/release.json" ] || fail 'release rollback failure unexpectedly removed the release'
    fi
    unset FAKE_GH_FAIL_OPERATION
done

make_neither_state
export FAKE_GH_PAGINATED_ASSETS=1
export FAKE_GH_NULL_DIGEST=1
make_release_state
snapshot_state "$FIXTURE/before-pagination-null-digest"
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --dry-run
assert_state_restored "$FIXTURE/before-pagination-null-digest" "$STATE" 'paginated nullable digest inventory'
export FAKE_GH_RESPONSE_FAILURE_OPERATION=candidate-asset-upload
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_RESPONSE_FAILURE_OPERATION
assert_success_state
make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-pagination-null-digest-rollback"
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after candidate-upload
assert_state_restored "$FIXTURE/before-pagination-null-digest-rollback" "$STATE" 'paginated nullable digest rollback'
unset FAKE_GH_PAGINATED_ASSETS
unset FAKE_GH_NULL_DIGEST

for response_operation in release-create old-asset-delete candidate-asset-upload; do
    make_neither_state
    case "$response_operation" in
        old-asset-delete)
            make_release_state
            ;;
    esac
    export FAKE_GH_RESPONSE_FAILURE_OPERATION=$response_operation
    assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
    unset FAKE_GH_RESPONSE_FAILURE_OPERATION
    assert_success_state
done

make_neither_state
make_release_state
export FAKE_GH_DELETE_404_OPERATION=old-asset-delete
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_DELETE_404_OPERATION
assert_success_state

make_neither_state
make_release_state
export FAKE_GH_CONCURRENT_OPERATION=state-inventory-assets
export FAKE_GH_CONCURRENT_AFTER=4
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_CONCURRENT_OPERATION
unset FAKE_GH_CONCURRENT_AFTER
[ ! -f "$STATE/tag.json" ] || fail 'concurrent mutation rollback overwrote the tag baseline'
jq -e '.name == "actor release"' "$STATE/release.json" >/dev/null \
    || fail 'concurrent mutation rollback overwrote the actor release'
[ -f "$STATE/assets/actor.bytes" ] || fail 'concurrent rollback deleted the actor asset'

make_neither_state
export FAKE_GH_CONCURRENT_OPERATION=state-inventory-tag
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_CONCURRENT_OPERATION
jq -e '.object.sha == "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"' "$STATE/tag.json" >/dev/null \
    || fail 'pre-tag conflict did not preserve the actor tag'
[ ! -f "$STATE/release.json" ] || fail 'pre-tag conflict created a release'

make_neither_state
make_release_state
export FAKE_GH_CONCURRENT_OPERATION=state-inventory-release
export FAKE_GH_CONCURRENT_AFTER=2
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_CONCURRENT_OPERATION
unset FAKE_GH_CONCURRENT_AFTER
[ ! -f "$STATE/tag.json" ] || fail 'between-mutation conflict did not roll back the candidate tag'
jq -e '.name == "actor release"' "$STATE/release.json" >/dev/null \
    || fail 'between-mutation conflict overwrote the actor release'

make_neither_state
export FAKE_GH_REQUIRE_ETAG=1
export FAKE_GH_RACE_OPERATION=tag-create
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_REQUIRE_ETAG
unset FAKE_GH_RACE_OPERATION
assert_eq "$(<"$STATE/precondition-failure")" 'resource already exists'
jq -e '.object.sha == "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"' "$STATE/tag.json" >/dev/null \
    || fail 'tag race did not preserve the actor tag'
[ ! -f "$STATE/release.json" ] || fail 'tag race created a release'

make_neither_state
make_release_state
export FAKE_GH_REQUIRE_ETAG=1
export FAKE_GH_RACE_OPERATION=release-update
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_REQUIRE_ETAG
unset FAKE_GH_RACE_OPERATION
assert_eq "$(<"$STATE/precondition-failure")" 'etag mismatch'
[ ! -f "$STATE/tag.json" ] || fail 'release race did not roll back the candidate tag'
jq -e '.name == "actor release"' "$STATE/release.json" >/dev/null \
    || fail 'release race overwrote the actor release'

make_neither_state
export FAKE_GH_REQUIRE_ETAG=1
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_REQUIRE_ETAG
[ ! -f "$STATE/precondition-failure" ] || fail 'publisher attempted a mutation without a conditional lease'
assert_success_state

make_neither_state
make_release_state
export FAKE_GH_REQUIRE_ETAG=1
assert_status 0 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_REQUIRE_ETAG
[ ! -f "$STATE/precondition-failure" ] || fail 'release asset mutation lacked a conditional lease'
assert_success_state

make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-etag-old-asset-rollback"
export FAKE_GH_REQUIRE_ETAG=1
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after old-asset-delete
unset FAKE_GH_REQUIRE_ETAG
assert_state_restored "$FIXTURE/before-etag-old-asset-rollback" "$STATE" 'ETag old-asset rollback'

make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-etag-candidate-rollback"
export FAKE_GH_REQUIRE_ETAG=1
assert_status 4 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after candidate-upload
unset FAKE_GH_REQUIRE_ETAG
assert_state_restored "$FIXTURE/before-etag-candidate-rollback" "$STATE" 'ETag candidate rollback'

make_neither_state
printf '{"ref":"refs/tags/unstable","object":{"type":"tag","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}\n' > "$STATE/tag.json"
export FAKE_GH_FAIL_OPERATION=candidate-asset-upload
export FAKE_GH_CONCURRENT_OPERATION=tag-rollback-inventory
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_FAIL_OPERATION
unset FAKE_GH_CONCURRENT_OPERATION
[ ! -f "$STATE/tag.json" ] || fail 'baseline tag deletion was overwritten during rollback'

make_neither_state
make_release_state
snapshot_state "$FIXTURE/before-baseline-asset-deletion"
export FAKE_GH_FAIL_OPERATION=release-update
export FAKE_GH_CONCURRENT_OPERATION=rollback-inventory-assets
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_FAIL_OPERATION
unset FAKE_GH_CONCURRENT_OPERATION
[ ! -f "$STATE/assets/legacy.json" ] || fail 'baseline asset deletion was overwritten during rollback'

make_neither_state
make_release_state
export FAKE_GH_REQUIRE_ETAG=1
export FAKE_GH_RACE_OPERATION=release-rollback-update
assert_status 6 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE" --fault-after candidate-upload
unset FAKE_GH_REQUIRE_ETAG
unset FAKE_GH_RACE_OPERATION
jq -e '.name == "actor release"' "$STATE/release.json" >/dev/null \
    || fail 'release ETag conflict was overwritten during rollback'

make_neither_state
snapshot_state "$FIXTURE/before-signal-during-rollback"
export FAKE_GH_FAIL_OPERATION=candidate-asset-upload
export FAKE_GH_SIGNAL_OPERATION=tag-rollback-inventory
signal_output="$FIXTURE/signal-during-rollback.log"
assert_status 4 run_publisher_capture "$signal_output" --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_FAIL_OPERATION
unset FAKE_GH_SIGNAL_OPERATION
grep -F 'signal TERM received during rollback; preserving publication status 4' "$signal_output" >/dev/null \
    || fail 'signal during rollback was not reported after rollback'
assert_state_restored "$FIXTURE/before-signal-during-rollback" "$STATE" 'signal during rollback'

make_neither_state
export FAKE_GH_SIGNAL_OPERATION=tag-create
assert_status 143 run_publisher --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
unset FAKE_GH_SIGNAL_OPERATION
[ ! -f "$STATE/tag.json" ] || fail 'signal rollback left the candidate tag'
[ ! -f "$STATE/release.json" ] || fail 'signal rollback created a release'

make_neither_state
export PUBLISH_TEST_COMPLETION_FILE="$FIXTURE/publication-complete"
(
    cd "$PUBLIC_CHECK" && exec "$SCRIPT" --repo owner/repo --sha "$CANDIDATE_SHA" --assets-dir "$CANDIDATE"
) &
publisher_pid=$!
completed=0
wait_count=0
while [ "$wait_count" -lt 500 ]; do
    if [ -f "$PUBLISH_TEST_COMPLETION_FILE" ]; then
        completed=1
        break
    fi
    sleep 0.01
    wait_count=$((wait_count + 1))
done
[ "$completed" -eq 1 ] || {
    kill -TERM "$publisher_pid" 2>/dev/null || true
    wait "$publisher_pid" 2>/dev/null || true
    fail 'publisher did not expose its completion hold'
}
kill -TERM "$publisher_pid"
set +e
wait "$publisher_pid"
signal_status=$?
set -e
assert_eq "$signal_status" 143
unset PUBLISH_TEST_COMPLETION_FILE
assert_success_state

printf '%s\n' 'publish-unstable fixture tests passed'
