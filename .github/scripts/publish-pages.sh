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
# record.
#
# # Why the push is a compare-and-swap, and not a lock
#
# An orphan commit replaces the whole tree, so a plain force-push drops
# whatever another writer put there between our fetch and our push — a
# preview published, or one taken down — with no error and no trace.
#
# This used to lean on an Actions concurrency group, and that is not a queue:
# `cancel-in-progress: false` protects the job that is *running*, and a third
# member joining the group cancels the one that was pending. A deployment
# could be discarded, and a preview's only close event could be dropped for
# good. Actions has no FIFO to borrow.
#
# So the branch itself is the lock. `--force-with-lease` fails if `gh-pages`
# is no longer where we found it, and the whole read-modify-write runs again
# against the new tip. Nothing is serialized, nothing is cancelled, and the
# last writer wins by re-reading rather than by overwriting.
set -euo pipefail

remove=false
if [ "${1:-}" = "--remove" ]; then
    remove=true
    shift
fi

# Something to re-ask before each attempt, when "should this happen at all" is
# a question the branch cannot answer.
#
# The lease below keeps two writers from losing each other's work; it says
# nothing about whether the work is still wanted. A pull request that closes
# while a publish is in flight is exactly that: the close job removes the
# preview, this one re-reads the branch it just changed, and — with no second
# question — puts the preview straight back. Asking once before the loop would
# not close it either; the window is between the question and the push, so the
# question belongs inside.
precondition=''
if [ "${1:-}" = "--only-if" ]; then
    precondition="${2:?--only-if needs a command}"
    shift 2
fi
target="${1:?a target directory is required}"

: "${GH_TOKEN:?a token is required to push}"
: "${GITHUB_REPOSITORY:?}"
: "${GITHUB_SERVER_URL:=https://github.com}"

branch=gh-pages
remote="${GITHUB_SERVER_URL#https://}"
remote="https://x-access-token:${GH_TOKEN}@${remote}/${GITHUB_REPOSITORY}.git"

bundle_dir=$(pwd)/bundle

# Bounded: a retry only happens when somebody else published in the seconds
# we took, and each attempt makes that less likely rather than more. A run
# that loses six times in a row is a symptom, not a race to keep running.
attempts=6
attempt=1
while [ "$attempt" -le "$attempts" ]; do

if [ -n "$precondition" ] && ! sh -c "$precondition"; then
    echo "no longer wanted; not publishing $target"
    exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

git -C "$work" init -q
git -C "$work" config user.name "github-actions[bot]"
git -C "$work" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git -C "$work" remote add origin "$remote"

# The branch may not exist yet: the first publish creates it.
#
# `had` is what the push is allowed to overwrite. Empty means we found no
# branch, and the lease then says the ref must still not exist.
if git -C "$work" fetch -q --depth 1 origin "$branch" 2>/dev/null; then
    git -C "$work" checkout -q FETCH_HEAD
    had=$(git -C "$work" rev-parse FETCH_HEAD)
    # Detached, and deliberately: the commit below is an orphan either way.
else
    echo "no $branch yet; creating it"
    had=''
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
    [ -d "$bundle_dir" ] || { echo "no ./bundle to publish" >&2; exit 1; }
    if [ "$target" = "." ]; then
        # The site itself. Clear what the last build left, and *keep* `pr/`:
        # publishing main must not take every open pull request's preview
        # down with it. `rm -rf .` is refused by `rm` anyway, which is what
        # first made this case visible.
        find "$work" -mindepth 1 -maxdepth 1 \
            ! -name .git ! -name pr -exec rm -rf {} +
        cp -R "$bundle_dir/." "$work/"
    else
        rm -rf "${work:?}/$target"
        mkdir -p "$work/$target"
        cp -R "$bundle_dir/." "$work/$target/"
    fi
    message="Publish $target from ${GITHUB_SHA:-a build}"
fi

# Jekyll would reinterpret the tree and drop anything under a path segment
# beginning with an underscore. At the root, because that is where Pages
# looks for it.
touch "$work/.nojekyll"

if [ -z "$(git -C "$work" status --porcelain)" ] \
    && git -C "$work" rev-parse --verify -q HEAD >/dev/null; then
    echo "nothing changed"
    exit 0
fi

# One commit, no parent: see the note at the top.
git -C "$work" checkout -q --orphan published
git -C "$work" add -A
git -C "$work" commit -q -m "$message"

if git -C "$work" push -q \
    --force-with-lease="refs/heads/$branch${had:+:$had}" \
    origin "published:$branch"; then
    echo "published $target to $branch"
    exit 0
fi

echo "$branch moved while we were building the tree; re-reading (attempt $attempt)" >&2
rm -rf "$work"
attempt=$((attempt + 1))
done

echo "gave up after $attempts attempts: $branch is changing faster than a publish takes" >&2
exit 1
