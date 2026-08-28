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
#
# # Why the last writer is not necessarily the newest build
#
# The lease answers "has anyone else written here", and that is a different
# question from "is what I am about to write still the newest thing to say".
# A run whose source ref advanced while it was building holds a stale bundle;
# it will happily fetch the newer run's tip, build its tree on it, and push
# with that very tip as its lease. Every check passes and the live site rolls
# back — and stays rolled back, because nothing after it is wrong.
#
# Asking an API "am I still the tip" cannot close that: the answer is read
# before the push and can go out of date in between. Narrowing the gap is not
# closing it. So the ordering is written into the branch instead, one number
# per target under `.publish/`, and read from the same tree the lease is taken
# on: a publish that finds a *higher* number there is looking at work newer
# than its own and stands down. The decision and the compare-and-swap then
# see the same state, which is what makes it a decision rather than a guess.
#
# The number is `GITHUB_RUN_NUMBER`, which is monotonic per workflow, and
# every publisher here is the same workflow. Re-running an old run keeps its
# number, so a deliberate re-run of a superseded build stands down rather than
# republishing — which is the behaviour worth having by default, and the
# reason a rollback is a revert rather than a re-run.
set -euo pipefail

# Flags in any order, because two of the three call sites pass both and the
# positional version silently ignored whichever came second.
remove=false
precondition=''
ordinal=''
while [ $# -gt 0 ]; do
    case "$1" in
        --remove)
            remove=true
            shift
            ;;
        # Something to re-ask before each attempt, when "should this happen at
        # all" is a question the branch cannot answer.
        #
        # The lease below keeps two writers from losing each other's work; it
        # says nothing about whether the work is still wanted. A pull request
        # that closes while a publish is in flight is exactly that, and so is
        # one reopened while its teardown is in flight. Asking once before the
        # loop would not close either window — the gap is between the question
        # and the push — so the question belongs inside.
        # Which publish this is, in the order they were started. See the
        # note on the marker below.
        --ordinal)
            ordinal="${2:?--ordinal needs a number}"
            shift 2
            ;;
        --only-if)
            precondition="${2:?--only-if needs a command}"
            shift 2
            ;;
        *)
            break
            ;;
    esac
done

target="${1:?a target directory is required}"
: "${ordinal:?--ordinal is required: without it there is no way to tell an older publish from a newer one}"

# What this target is called in the marker: `site` for the page itself,
# `pr-12` for a preview. One claim per target, because the site and every
# preview are separate subtrees of one branch and none of them orders another.
slug=$(printf '%s' "$target" | sed -e 's:/*$::' -e 's:^\.$:site:' -e 's:/:-:g')

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

if [ -n "$precondition" ]; then
    set +e
    sh -c "$precondition"
    wanted=$?
    set -e
    case "$wanted" in
        0) ;;
        # 1 superseded, 2 no longer the desired state. Both are answers, and
        # standing down is the right thing to do about them.
        1 | 2)
            echo "no longer wanted (check said $wanted); not touching $branch"
            exit 0
            ;;
        # Anything else is the check *breaking* — a network blip, a rate
        # limit, an API 500 — and reading that as "stand down" is how a
        # deployment silently does not happen while the job reports success
        # and leaves nothing to retry. An unanswered question is not a no.
        *)
            echo "the current-state check failed with $wanted; refusing to guess" >&2
            exit 1
            ;;
    esac
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

# Read from the tree we just fetched, which is the tree the lease is taken
# against: whatever is here is what the push would replace. A higher number
# is somebody else's newer work, and overwriting it is the rollback this
# exists to prevent.
#
# An unreadable or absent claim is no claim — the first publish makes one, and
# a corrupted one should not wedge publishing for good.
held=0
if [ -f "$work/.publish/$slug" ]; then
    held=$(cat "$work/.publish/$slug")
    case "$held" in
        '' | *[!0-9]*) held=0 ;;
    esac
fi
if [ "$held" -gt "$ordinal" ]; then
    echo "a newer publish ($held) already holds $target; standing down"
    exit 0
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
            ! -name .git ! -name pr ! -name .publish -exec rm -rf {} +
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

# Claimed before anything asks whether this publish is a no-op, and that
# order is the point. Claiming only when the bundle differs looked thriftier
# and left a hole: a newer run that happens to build byte-identical output —
# a revert, a rebuild — would exit with the *old* number still standing, and
# a slower run from before it, carrying different output, would then read that
# old claim, pass the check, and publish over the newer one. The claim is
# content, and it has to move whenever the run that owns the branch moves.
#
# A removal claims too. Without it a publish still in flight from before the
# pull request closed would find no claim, put the preview back, and leave it
# there for good — the teardown is a single event and does not come again.
mkdir -p "$work/.publish"
printf '%s\n' "$ordinal" > "$work/.publish/$slug"

# Nothing to say *and* nothing to claim: the same run publishing the same
# bundle again, which is a re-run rather than a race.
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
    if [ "$remove" = true ]; then
        echo "removed $target from $branch"
    else
        echo "published $target to $branch"
    fi
    exit 0
fi

echo "$branch moved while we were building the tree; re-reading (attempt $attempt)" >&2
rm -rf "$work"
attempt=$((attempt + 1))
done

echo "gave up after $attempts attempts: $branch is changing faster than a publish takes" >&2
exit 1
