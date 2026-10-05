#!/usr/bin/env bash
# Publish the committed Cargo version after CI passes on GitHub.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$root"
repo=demfabris/zflow
dry_run=false
wait_for_release=true

die() { printf 'zflow release: %s\n' "$*" >&2; exit 1; }

for argument in "$@"; do
    case "$argument" in
        --dry-run) dry_run=true ;;
        --no-wait) wait_for_release=false ;;
        -h|--help)
            printf 'Usage: scripts/release.sh [--dry-run] [--no-wait]\n'
            printf 'Release the committed Cargo.toml version from a clean main branch.\n'
            printf 'Push main, wait for CI, push its annotated tag, then wait for publication.\n'
            printf '%s\n' '--dry-run checks without pushing or tagging; --no-wait skips waiting for publication.'
            exit 0
            ;;
        *) die "Unknown option: $argument" ;;
    esac
done

for tool in git gh python3; do command -v "$tool" >/dev/null || die "Missing command: $tool"; done
[[ "$(git symbolic-ref --quiet --short HEAD)" == main ]] || die 'Switch to main before releasing.'
[[ -z "$(git status --porcelain)" ]] || die 'Commit or set aside all working-tree changes before releasing.'
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die 'Use a stable major.minor.patch version in Cargo.toml.'
tag="v$version"
commit=$(git rev-parse HEAD)
[[ "$(gh repo view --json nameWithOwner --jq .nameWithOwner)" == "$repo" ]] \
    || die "The repository must be $repo."
remote_url=$(git remote get-url --push origin)
case "$remote_url" in
    git@github.com:demfabris/zflow.git|git@github.com:demfabris/zflow|https://github.com/demfabris/zflow.git|https://github.com/demfabris/zflow) ;;
    *) die 'origin must push to demfabris/zflow on GitHub.' ;;
esac
public_key=$(gh variable list --repo "$repo" --json name,value \
    --jq '.[] | select(.name == "SPARKLE_PUBLIC_ED_KEY") | .value') \
    || die 'Run just release setup on a Mac before publishing updates.'
[[ -n "$public_key" ]] || die 'SPARKLE_PUBLIC_ED_KEY is empty; run just release setup on a Mac.'
secret_names=$(gh secret list --repo "$repo" --json name --jq '.[].name')
printf '%s\n' "$secret_names" | grep -qx SPARKLE_PRIVATE_ED_KEY \
    || die 'SPARKLE_PRIVATE_ED_KEY is missing; run just release setup on a Mac.'

# Read the remote without changing local tracking refs, including during --dry-run.
remote_refs=$(git ls-remote origin refs/heads/main "refs/tags/$tag")
[[ -z "$(printf '%s\n' "$remote_refs" | awk -v tag="refs/tags/$tag" '$2 == tag {print $1}')" ]] \
    || die "$tag already exists on GitHub. Inspect its release workflow instead of retagging it."
remote_main=$(printf '%s\n' "$remote_refs" | awk '$2 == "refs/heads/main" {print $1}')
[[ -n "$remote_main" ]] || die 'origin/main is missing.'
git merge-base --is-ancestor "$remote_main" "$commit" \
    || die 'Fetch origin and bring main up to date before releasing.'
if git show-ref --verify --quiet "refs/tags/$tag"; then
    [[ "$(git cat-file -t "refs/tags/$tag")" == tag && "$(git rev-parse "$tag^{commit}")" == "$commit" ]] \
        || die "Local $tag must be an annotated tag at the release commit."
fi
latest=$(gh release view --repo "$repo" --json tagName --jq .tagName)
python3 - "$version" "$latest" <<'PY'
import re
import sys

version, latest = sys.argv[1:]
if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", latest):
    sys.exit("Cannot compare the latest published version: " + latest)
if tuple(map(int, version.split('.'))) <= tuple(map(int, latest[1:].split('.'))):
    sys.exit("The release version must be newer than " + latest)
PY
printf 'Release %s from %s (latest published: %s).\n' "$tag" "$commit" "$latest"
if [[ "$dry_run" == true ]]; then
    printf 'Would push main, require successful CI for this commit, push %s, and wait for Release.\n' "$tag"
    exit 0
fi

wait_for_run() {
    local workflow=$1 event=$2 branch=$3 run_id attempt
    for ((attempt = 0; attempt < 30; attempt++)); do
        run_id=$(gh run list --repo "$repo" --workflow "$workflow" --commit "$commit" \
            --event "$event" --branch "$branch" --limit 1 --json databaseId --jq '.[0].databaseId // empty')
        if [[ -n "$run_id" ]]; then
            gh run watch "$run_id" --repo "$repo" --exit-status --interval 10
            return
        fi
        sleep 2
    done
    die "No $workflow run appeared for $commit. Check GitHub Actions before retrying."
}

git push origin "$commit:refs/heads/main"
wait_for_run ci.yml push main
# Do not tag a different checkout if another process changed it while CI ran.
[[ "$(git rev-parse HEAD)" == "$commit" && -z "$(git status --porcelain)" ]] \
    || die 'The checkout changed while CI ran; no release tag was pushed.'
if ! git show-ref --verify --quiet "refs/tags/$tag"; then
    git tag --annotate "$tag" "$commit" --message "zflow $tag"
fi
tag_object=$(git rev-parse "refs/tags/$tag")
[[ "$(git cat-file -t "$tag_object")" == tag && "$(git rev-parse "$tag_object^{commit}")" == "$commit" ]] \
    || die 'The release tag changed while CI ran; no tag was pushed.'
git push origin "$tag_object:refs/tags/$tag"
printf 'Release queued: https://github.com/%s/actions/workflows/release.yml\n' "$repo"
if [[ "$wait_for_release" == true ]]; then
    wait_for_run release.yml push "$tag"
    gh release view "$tag" --repo "$repo" --json url --jq .url
fi
