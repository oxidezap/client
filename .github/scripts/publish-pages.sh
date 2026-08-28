#!/usr/bin/env bash
# Put a built bundle on the `gh-pages` branch, or take one off it.
#
#     publish-pages.sh <target>            copy ./bundle into <target>
#     publish-pages.sh --remove <target>   delete <target>
#
# `<target>` is `.` for the site itself and `pr/<n>/` for a preview, so one
# branch carries the live page and every open pull request's copy of it.
#
# # Why the branch is rewritten rather than added to
#
# A bundle is about twenty megabytes, and git keeps every blob it has ever
# seen. A preview per pull request on an ordinary history would grow the
# repository by that much per push, for ever, and deleting the directory
# afterwards would not give any of it back. So each publish writes a single
# orphan commit holding the whole tree: the previous commit becomes
# unreachable and its blobs are collectable, and the branch stays one commit
# deep. Nothing reads `gh-pages` history — it is a publishing surface, not a
# record — and the workflow serializes on one concurrency group, so there is
# no second writer whose commit a force-push could drop.
set -euo pipefail

remove=false
if [ "${1:-}" = "--remove" ]; then
    remove=true
    shift
fi
target="${1:?a target directory is required}"

: "${GH_TOKEN:?a token is required to push}"
: "${GITHUB_REPOSITORY:?}"
: "${GITHUB_SERVER_URL:=https://github.com}"

branch=gh-pages
remote="${GITHUB_SERVER_URL#https://}"
remote="https://x-access-token:${GH_TOKEN}@${remote}/${GITHUB_REPOSITORY}.git"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

git -C "$work" init -q
git -C "$work" config user.name "github-actions[bot]"
git -C "$work" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git -C "$work" remote add origin "$remote"

# The branch may not exist yet: the first publish creates it.
if git -C "$work" fetch -q --depth 1 origin "$branch" 2>/dev/null; then
    git -C "$work" checkout -q FETCH_HEAD
    # Detached, and deliberately: the commit below is an orphan either way.
else
    echo "no $branch yet; creating it"
fi

if [ "$remove" = true ]; then
    if [ ! -d "$work/$target" ]; then
        echo "nothing to remove at $target"
        exit 0
    fi
    rm -rf "${work:?}/$target"
    # An empty `pr/` left behind is not wrong, but it is litter.
    rmdir "$work/$(dirname "$target")" 2>/dev/null || true
    message="Remove the preview at $target"
else
    [ -d bundle ] || { echo "no ./bundle to publish" >&2; exit 1; }
    if [ "$target" = "." ]; then
        # The site itself. Clear what the last build left, and *keep* `pr/`:
        # publishing main must not take every open pull request's preview
        # down with it. `rm -rf .` is refused by `rm` anyway, which is what
        # first made this case visible.
        find "$work" -mindepth 1 -maxdepth 1 \
            ! -name .git ! -name pr -exec rm -rf {} +
        cp -R bundle/. "$work/"
    else
        rm -rf "${work:?}/$target"
        mkdir -p "$work/$target"
        cp -R bundle/. "$work/$target/"
    fi
    message="Publish $target from ${GITHUB_SHA:-a build}"
fi

# Jekyll would reinterpret the tree and drop anything under a path segment
# beginning with an underscore. At the root, because that is where Pages
# looks for it.
touch "$work/.nojekyll"

cd "$work"
if [ -z "$(git status --porcelain)" ] && git rev-parse --verify -q HEAD >/dev/null; then
    echo "nothing changed"
    exit 0
fi

# One commit, no parent: see the note at the top.
git checkout -q --orphan published
git add -A
git commit -q -m "$message"
git push -q --force origin "published:$branch"
echo "published $target to $branch"
