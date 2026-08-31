//! Put a built bundle on the `gh-pages` branch, or take one off it.
//!
//! `<target>` is `.` for the site itself and `pr/<n>` for a preview, so one
//! branch carries the live page and every open pull request's copy of it.
//!
//! # Why the branch is rewritten rather than added to
//!
//! A bundle is about twenty megabytes, and git keeps every blob it has ever
//! seen. A preview per pull request on an ordinary history would grow the
//! repository by that much per push, for ever, and deleting the directory
//! afterwards would not give any of it back. So each publish writes a single
//! orphan commit holding the whole tree: the previous commit becomes
//! unreachable and its blobs are collectable, and the branch stays one commit
//! deep. Nothing reads `gh-pages` history — it is a publishing surface, not a
//! record.
//!
//! # Why the push is a compare-and-swap, and not a lock
//!
//! An orphan commit replaces the whole tree, so a plain force-push drops
//! whatever another writer put there between our fetch and our push — a
//! preview published, or one taken down — with no error and no trace.
//!
//! This used to lean on an Actions concurrency group, and that is not a
//! queue: `cancel-in-progress: false` protects the job that is *running*, and
//! a third member joining the group cancels the one that was pending. A
//! deployment could be discarded, and a preview's only close event could be
//! dropped for good. Actions has no FIFO to borrow.
//!
//! So the branch itself is the lock. `--force-with-lease` fails if `gh-pages`
//! is no longer where we found it, and the whole read-modify-write runs again
//! against the new tip. Nothing is serialized, nothing is cancelled, and the
//! last writer wins by re-reading rather than by overwriting.
//!
//! # Why the last writer is not necessarily the newest build
//!
//! The lease answers "has anyone else written here", and that is a different
//! question from "is what I am about to write still the newest thing to say".
//! A run whose source ref advanced while it was building holds a stale bundle;
//! it will happily fetch the newer run's tip, build its tree on it, and push
//! with that very tip as its lease. Every check passes and the live site rolls
//! back — and stays rolled back, because nothing after it is wrong.
//!
//! Asking an API "am I still the tip" cannot close that: the answer is read
//! before the push and can go out of date in between. Narrowing the gap is not
//! closing it. So the ordering is written into the branch instead, one number
//! per target under `.publish/`, and read from the same tree the lease is
//! taken on: a publish that finds a *higher* number there is looking at work
//! newer than its own and stands down. The decision and the compare-and-swap
//! then see the same state, which is what makes it a decision rather than a
//! guess.
//!
//! The number is `GITHUB_RUN_NUMBER`, which is monotonic per workflow, and
//! every publisher here is the same workflow. Re-running an old run keeps its
//! number, so a deliberate re-run of a superseded build stands down rather
//! than republishing — which is the behaviour worth having by default, and the
//! reason a rollback is a revert rather than a re-run.

use std::fs;
use std::path::{Path, PathBuf};

use crate::check::{Api, Precondition, Wanted};
use crate::util::{Result, Run, TempDir, copy_dir_contents, env_or, need_env, remove};
use crate::{err, note, say};

const BRANCH: &str = "gh-pages";

/// Bounded: a retry only happens when somebody else published in the seconds
/// we took, and each attempt makes that less likely rather than more. A run
/// that loses six times in a row is a symptom, not a race to keep running.
const ATTEMPTS: u32 = 6;

/// What a run is doing to one directory of the branch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Publish,
    Remove,
}

pub struct Job {
    pub action: Action,
    pub target: String,
    /// Which publish this is, in the order they were started.
    pub ordinal: u64,
    pub check: Precondition,
    /// The tree to publish. Only read by `Publish`.
    pub bundle: PathBuf,
}

/// Where the branch lives, and what to stamp on the commit.
///
/// A parameter rather than something `run` reads out of the environment, so
/// the read-modify-write can be driven against a repository on disk — which
/// is what the tests at the bottom of this file do, and what the shell
/// version had no way of offering.
pub struct Remote {
    pub url: String,
    /// What the publish commit says it came from.
    pub source: String,
}

impl Remote {
    /// The `gh-pages` of the repository this run belongs to, with a token in
    /// it because that is how Actions authenticates a push.
    pub fn from_env() -> Result<Self> {
        let token = need_env("GH_TOKEN")?;
        let repository = need_env("GITHUB_REPOSITORY")?;
        let server = env_or("GITHUB_SERVER_URL", "https://github.com");
        let host = server.strip_prefix("https://").unwrap_or(&server);
        Ok(Remote {
            url: format!("https://x-access-token:{token}@{host}/{repository}.git"),
            source: env_or("GITHUB_SHA", "a build"),
        })
    }
}

/// Run the read-modify-write until it lands, is refused, or runs out of
/// attempts.
pub fn run(job: &Job, remote: &Remote) -> Result<()> {
    let target = normalize_target(&job.target);
    let slug = slug_for(&target);

    // Read once: a missing variable should be one message before the first
    // network call, not a surprise on attempt four.
    let api = match job.check {
        Precondition::Always => None,
        _ => Some(Api::from_env()?),
    };

    for attempt in 1..=ATTEMPTS {
        if let Some(api) = &api
            && job.check.ask(api)? == Wanted::StandDown
        {
            say!("no longer wanted; not touching {BRANCH}");
            return Ok(());
        }

        let work = TempDir::new("oxidezap-pages")?;
        match attempt_once(job, work.path(), remote, &target, &slug)? {
            Outcome::Done => return Ok(()),
            Outcome::LostTheLease => {
                note!(
                    "{BRANCH} moved while we were building the tree; re-reading (attempt {attempt})"
                );
            }
        }
    }

    Err(err!(
        "gave up after {ATTEMPTS} attempts: {BRANCH} is changing faster than a publish takes"
    ))
}

enum Outcome {
    Done,
    LostTheLease,
}

fn attempt_once(
    job: &Job,
    work: &Path,
    remote: &Remote,
    target: &str,
    slug: &str,
) -> Result<Outcome> {
    let git = |args: &[&str]| {
        Run::new("git")
            .arg("-C")
            .arg(work)
            .args(args.iter().copied())
    };

    git(&["init", "-q"]).run()?;
    git(&["config", "user.name", "github-actions[bot]"]).run()?;
    git(&[
        "config",
        "user.email",
        "41898282+github-actions[bot]@users.noreply.github.com",
    ])
    .run()?;
    Run::new("git")
        .arg("-C")
        .arg(work)
        .args(["remote", "add", "origin"])
        .arg(&remote.url)
        .run()?;

    // The branch may not exist yet: the first publish creates it.
    //
    // `had` is what the push is allowed to overwrite. `None` means we found no
    // branch, and the lease then says the ref must still not exist.
    let fetched = git(&["fetch", "-q", "--depth", "1", "origin", BRANCH])
        .output()?
        .status
        .success();
    let had = if fetched {
        git(&["checkout", "-q", "FETCH_HEAD"]).run()?;
        // Detached, and deliberately: the commit below is an orphan either way.
        Some(git(&["rev-parse", "FETCH_HEAD"]).read()?)
    } else {
        say!("no {BRANCH} yet; creating it");
        None
    };

    // Read from the tree we just fetched, which is the tree the lease is taken
    // against: whatever is here is what the push would replace. A higher
    // number is somebody else's newer work, and overwriting it is the rollback
    // this exists to prevent.
    //
    // An unreadable or absent claim is no claim — the first publish makes one,
    // and a corrupted one should not wedge publishing for good.
    let held = read_claim(&work.join(".publish").join(slug));
    if held > job.ordinal {
        say!("a newer publish ({held}) already holds {target}; standing down");
        return Ok(Outcome::Done);
    }

    let message = match job.action {
        Action::Remove => remove_target(work, target)?,
        Action::Publish => publish_target(job, work, target, &remote.source)?,
    };

    // Jekyll would reinterpret the tree and drop anything under a path segment
    // beginning with an underscore. At the root, because that is where Pages
    // looks for it.
    fs::write(work.join(".nojekyll"), b"")?;

    // Claimed before anything asks whether this publish is a no-op, and that
    // order is the point. Claiming only when the bundle differs looked
    // thriftier and left a hole: a newer run that happens to build
    // byte-identical output — a revert, a rebuild — would exit with the *old*
    // number still standing, and a slower run from before it, carrying
    // different output, would then read that old claim, pass the check, and
    // publish over the newer one. The claim is content, and it has to move
    // whenever the run that owns the branch moves.
    //
    // A removal claims too. Without it a publish still in flight from before
    // the pull request closed would find no claim, put the preview back, and
    // leave it there for good — the teardown is a single event and does not
    // come again.
    let claims = work.join(".publish");
    fs::create_dir_all(&claims)?;
    fs::write(claims.join(slug), format!("{}\n", job.ordinal))?;

    // Nothing to say *and* nothing to claim: the same run publishing the same
    // bundle again, which is a re-run rather than a race.
    let dirty = !git(&["status", "--porcelain"]).read()?.is_empty();
    let has_head = git(&["rev-parse", "--verify", "-q", "HEAD"])
        .output()?
        .status
        .success();
    if !dirty && has_head {
        say!("nothing changed");
        return Ok(Outcome::Done);
    }

    // One commit, no parent: see the note at the top.
    git(&["checkout", "-q", "--orphan", "published"]).run()?;
    git(&["add", "-A"]).run()?;
    Run::new("git")
        .arg("-C")
        .arg(work)
        .args(["commit", "-q", "-m"])
        .arg(&message)
        .run()?;

    let lease = match &had {
        Some(had) => format!("refs/heads/{BRANCH}:{had}"),
        None => format!("refs/heads/{BRANCH}"),
    };
    let pushed = Run::new("git")
        .arg("-C")
        .arg(work)
        .args(["push", "-q"])
        .arg(format!("--force-with-lease={lease}"))
        .args(["origin", &format!("published:{BRANCH}")])
        .status()?;

    if pushed == Some(0) {
        match job.action {
            Action::Remove => say!("removed {target} from {BRANCH}"),
            Action::Publish => say!("published {target} to {BRANCH}"),
        }
        return Ok(Outcome::Done);
    }
    Ok(Outcome::LostTheLease)
}

fn remove_target(work: &Path, target: &str) -> Result<String> {
    let path = work.join(target);
    if path.is_dir() {
        remove(&path)?;
        // An empty `pr/` left behind is not wrong, but it is litter.
        if let Some(parent) = Path::new(target).parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::remove_dir(work.join(parent));
        }
        Ok(format!("Remove the preview at {target}"))
    } else {
        // Nothing to delete, and that is precisely when the claim is
        // load-bearing. A pull request closed before its first publisher
        // reached this branch finds no directory here — and leaving without
        // recording the removal's ordinal lets that publisher, still building,
        // read no claim, pass the check and create the closed pull request's
        // preview. Nothing comes back for it: a teardown is a single event.
        //
        // So the removal is claimed whether or not it removed anything. What
        // this branch says is not "a directory was deleted" but "this ordinal
        // owns this target, and it owns nothing".
        say!("nothing to remove at {target}; recording the removal anyway");
        Ok(format!("Claim the removal of {target}"))
    }
}

fn publish_target(job: &Job, work: &Path, target: &str, source: &str) -> Result<String> {
    if !job.bundle.is_dir() {
        return Err(err!("no {} to publish", job.bundle.display()));
    }
    if target == "." {
        // The site itself. Clear what the last build left, and *keep* `pr/`:
        // publishing main must not take every open pull request's preview down
        // with it.
        for entry in fs::read_dir(work)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | "pr" | ".publish")) {
                continue;
            }
            remove(&entry.path())?;
        }
        copy_dir_contents(&job.bundle, work)?;
    } else {
        let into = work.join(target);
        remove(&into)?;
        fs::create_dir_all(&into)?;
        copy_dir_contents(&job.bundle, &into)?;
        mark_as_preview(&into.join("index.html"))?;
    }
    Ok(format!("Publish {target} from {source}"))
}

/// A preview says what it is, because the page has to know.
///
/// It shares an origin with the deployment — same scheme, same host, one
/// directory over — and a page that runs its own session keeps the account in
/// storage scoped to that origin. Unmerged code one path segment away could
/// read it, token or no token, so a preview refuses to hold an account at all
/// and attaches to a named daemon instead. Declared here rather than guessed
/// from the URL, because the page deciding by its own path is a rule that
/// breaks the first time somebody serves this somewhere else.
fn mark_as_preview(index: &Path) -> Result<()> {
    const TAG: &str = r#"<meta name="oxidezap-build" content="preview" />"#;
    let html =
        fs::read_to_string(index).map_err(|e| err!("could not read {}: {e}", index.display()))?;

    // `sed` exits 0 when it matches nothing, and the page reads the absence of
    // this tag as "not a preview" — which is the direction that opens a
    // session in the deployment's own origin storage. So the injection is
    // *checked* rather than assumed: a bundler that emits `<head >`, `<HEAD>`
    // or no literal `<head>` at all must fail the publish here, loudly, rather
    // than ship a preview that holds an account.
    let Some(at) = html.find("<head>") else {
        return Err(err!(
            "could not mark {} as a preview: no <head> to inject into",
            index.display()
        ));
    };
    let mut marked = String::with_capacity(html.len() + TAG.len());
    marked.push_str(&html[..at + "<head>".len()]);
    marked.push_str(TAG);
    marked.push_str(&html[at + "<head>".len()..]);
    fs::write(index, &marked).map_err(|e| err!("could not write {}: {e}", index.display()))?;
    Ok(())
}

/// What this target is called in the marker: `site` for the page itself,
/// `pr-12` for a preview. One claim per target, because the site and every
/// preview are separate subtrees of one branch and none of them orders
/// another.
fn slug_for(target: &str) -> String {
    let trimmed = target.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return "site".to_string();
    }
    trimmed.replace('/', "-")
}

/// `pr/12/` and `pr/12` are one target; `.` and `` are the site.
fn normalize_target(target: &str) -> String {
    let trimmed = target.trim_end_matches('/');
    if trimmed.is_empty() {
        ".".to_string()
    } else {
        trimmed.to_string()
    }
}

fn read_claim(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{Action, Job, Remote, mark_as_preview, normalize_target, read_claim, slug_for};
    use crate::check::Precondition;
    use crate::util::{Result, Run, TempDir};

    #[test]
    fn the_site_and_a_preview_get_different_slugs() {
        assert_eq!(slug_for("."), "site");
        assert_eq!(slug_for("./"), "site");
        assert_eq!(slug_for("pr/12"), "pr-12");
        assert_eq!(slug_for("pr/12/"), "pr-12");
    }

    #[test]
    fn a_trailing_slash_does_not_make_a_second_target() {
        assert_eq!(normalize_target("pr/12/"), "pr/12");
        assert_eq!(normalize_target("."), ".");
        assert_eq!(normalize_target(""), ".");
    }

    /// An unreadable or absent claim is no claim: the first publish makes one,
    /// and a corrupted one must not wedge publishing for good.
    #[test]
    fn a_claim_that_is_not_a_number_is_no_claim() {
        let dir = crate::util::TempDir::new("xtask-claim").unwrap();
        let path = dir.path().join("claim");
        assert_eq!(read_claim(&path), 0);
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_claim(&path), 0);
        std::fs::write(&path, "not a number\n").unwrap();
        assert_eq!(read_claim(&path), 0);
        std::fs::write(&path, "  41 \n").unwrap();
        assert_eq!(read_claim(&path), 41);
    }

    /// The page reads the absence of this tag as "not a preview", which is the
    /// direction that opens a session in the deployment's own origin storage.
    /// So an `index.html` this cannot mark must fail the publish rather than
    /// ship.
    #[test]
    fn a_preview_that_cannot_be_marked_is_refused() {
        let dir = TempDir::new("xtask-preview").unwrap();
        let index = dir.path().join("index.html");

        fs::write(&index, "<html><head><title>x</title></head></html>").unwrap();
        mark_as_preview(&index).unwrap();
        let marked = fs::read_to_string(&index).unwrap();
        assert!(marked.contains(r#"<meta name="oxidezap-build" content="preview" />"#));
        assert!(marked.contains("<title>x</title>"));

        // The three shapes the shell's `sed` would have passed silently.
        for html in [
            "<html><head ></head></html>",
            "<HTML><HEAD></HEAD>",
            "<html></html>",
        ] {
            fs::write(&index, html).unwrap();
            assert!(mark_as_preview(&index).is_err(), "{html} should be refused");
        }
    }

    /// The whole read-modify-write, against a repository on disk.
    ///
    /// Nothing here reaches the network: `Remote` is a parameter, so `origin`
    /// can be a bare repository in a temporary directory. What it holds to is
    /// the four things the branch's design is: the site and a preview live
    /// side by side, publishing the site keeps every preview, an older run
    /// stands down rather than rolling the site back, and a removal claims its
    /// ordinal whether or not it found anything.
    #[test]
    fn the_branch_carries_the_site_and_its_previews() -> Result<()> {
        let scratch = TempDir::new("xtask-pages")?;
        let origin = scratch.path().join("origin.git");
        Run::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&origin)
            .run()?;

        let bundle = scratch.path().join("bundle");
        write_bundle(&bundle, "one");

        let remote = Remote {
            url: origin.to_string_lossy().into_owned(),
            source: "abc123".to_string(),
        };
        let publish = |target: &str, ordinal: u64| Job {
            action: Action::Publish,
            target: target.to_string(),
            ordinal,
            check: Precondition::Always,
            bundle: bundle.clone(),
        };

        super::run(&publish(".", 1), &remote)?;
        let tree = checkout(&scratch.path().join("read-1"), &origin)?;
        assert_eq!(read(&tree, "index.html"), "<html><head></head>one</html>");
        assert_eq!(read(&tree, ".publish/site"), "1\n");
        assert!(
            tree.join(".nojekyll").exists(),
            "Pages must not run Jekyll over this"
        );

        super::run(&publish("pr/12", 3), &remote)?;
        let tree = checkout(&scratch.path().join("read-2"), &origin)?;
        assert!(
            read(&tree, "pr/12/index.html").contains(r#"content="preview""#),
            "a preview must say what it is"
        );
        assert_eq!(read(&tree, ".publish/pr-12"), "3\n");
        assert_eq!(read(&tree, "index.html"), "<html><head></head>one</html>");

        // Publishing the site must not take an open pull request's preview
        // down with it.
        write_bundle(&bundle, "two");
        super::run(&publish(".", 4), &remote)?;
        let tree = checkout(&scratch.path().join("read-3"), &origin)?;
        assert_eq!(read(&tree, "index.html"), "<html><head></head>two</html>");
        assert!(tree.join("pr/12/index.html").exists(), "the preview stays");

        // A slower run from before it, carrying older output, must not roll
        // the live site back.
        write_bundle(&bundle, "stale");
        super::run(&publish(".", 2), &remote)?;
        let tree = checkout(&scratch.path().join("read-4"), &origin)?;
        assert_eq!(read(&tree, "index.html"), "<html><head></head>two</html>");
        assert_eq!(read(&tree, ".publish/site"), "4\n");

        // A removal claims its ordinal whether or not it found anything.
        let remove = |target: &str, ordinal: u64| Job {
            action: Action::Remove,
            target: target.to_string(),
            ordinal,
            check: Precondition::Always,
            bundle: bundle.clone(),
        };
        super::run(&remove("pr/12", 5), &remote)?;
        super::run(&remove("pr/99", 6), &remote)?;
        let tree = checkout(&scratch.path().join("read-5"), &origin)?;
        assert!(!tree.join("pr/12").exists(), "the preview is gone");
        assert!(
            !tree.join("pr").exists(),
            "and so is the empty directory it left"
        );
        assert_eq!(read(&tree, ".publish/pr-12"), "5\n");
        assert_eq!(
            read(&tree, ".publish/pr-99"),
            "6\n",
            "a teardown is a single event: it has to be recorded even when it \
             found nothing, or a publisher still in flight puts the preview back"
        );

        // One commit deep, always: a bundle per push on an ordinary history
        // would grow the repository by twenty megabytes for ever.
        let depth = Run::new("git")
            .arg("-C")
            .arg(scratch.path().join("read-5"))
            .args(["rev-list", "--count", "HEAD"])
            .read()?;
        assert_eq!(depth, "1");
        Ok(())
    }

    fn write_bundle(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("index.html"),
            format!("<html><head></head>{body}</html>"),
        )
        .unwrap();
    }

    fn checkout(into: &Path, origin: &Path) -> Result<std::path::PathBuf> {
        Run::new("git")
            .args(["clone", "-q", "--branch", "gh-pages"])
            .arg(origin)
            .arg(into)
            .run()?;
        Ok(into.to_path_buf())
    }

    fn read(tree: &Path, rel: &str) -> String {
        fs::read_to_string(tree.join(rel)).unwrap_or_else(|e| panic!("could not read {rel}: {e}"))
    }
}
