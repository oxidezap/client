//! Making sure what we pushed to `gh-pages` is what Pages actually serves.
//!
//! Everything in `pages.rs` is about the *branch*, and getting the branch
//! right turns out not to be the end of the job. A push to `gh-pages` starts
//! a run of GitHub's own `pages build and deployment` workflow, which is not
//! in this repository and cannot be configured from it, and the deployment it
//! creates is refused while another one is in flight:
//!
//! ```text
//! Deployment request failed for <sha> due to in progress deployment.
//! Please cancel <other> first or wait for it to complete.
//! ```
//!
//! Two writers landing within a minute of each other is the ordinary case
//! here, not a rare one — a pull request closing takes its preview down while
//! the merge that closed it publishes the site — and the lease that makes
//! *both* writes land is precisely what puts their two pushes close together.
//! So the second deployment 400s, and the branch is then carrying a tree that
//! nothing will ever serve: Pages deploys on a push, and the push has already
//! happened. For a preview take-down that is the end of the line, because a
//! pull request closes once. The preview stays live for good.
//!
//! Re-running the failed deployment does not fix it and makes the log worse:
//! the built-in workflow's build job uploads its artifact again into the same
//! run, and its deploy job then refuses a second time with
//! "Multiple artifacts named github-pages were unexpectedly found for this
//! workflow run" — which is the error people see, one step removed from the
//! collision that caused it.
//!
//! What closes it is asking, after the push, whether the branch tip is what
//! Pages last built, and asking for a build when it is not. That is a
//! *convergence* rather than a lock, for the same reason the branch write is:
//! there is no queue to join. Any run that finds the tip undeployed asks for
//! it, a legacy Pages build takes whatever the tip is at the moment it runs
//! rather than a commit chosen in advance, and a run whose push has since
//! been overtaken stands down and leaves the newer run to settle its own.

use std::thread::sleep;
use std::time::Duration;

use crate::check::{Api, Posted};
use crate::json;
use crate::util::{Result, Run};
use crate::{err, note, say};

/// How often to ask. A legacy Pages build of this bundle is tens of seconds,
/// so five is often enough to see each state change and rare enough to be
/// nothing next to the build it is watching.
const EVERY: Duration = Duration::from_secs(5);

/// How many times, at most. Seven and a half minutes: longer than any build
/// this repository has produced, and short enough that a Pages outage fails
/// the job rather than holding a runner for an hour.
const POLLS: u32 = 90;

/// How many builds one run may ask for. A build we asked for that comes back
/// errored is worth one more attempt; a third is a Pages problem, not a race.
const ASKS: u32 = 3;

/// How long to wait for a build of *something else* to start before deciding
/// the push did not start one. GitHub queues the automatic build a few
/// seconds after the push, and asking for one before it appears just spends
/// an ask on a build that was already coming.
const GRACE: u32 = 12;

/// The last build Pages made of this branch.
///
/// `commit` is which tree it was of, which is what makes "has our push been
/// served" answerable at all; `status` is `queued`, `building`, `built` or
/// `errored`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub status: String,
    pub commit: String,
}

impl Build {
    fn in_flight(&self) -> bool {
        self.status == "building" || self.status == "queued"
    }
}

/// What to do next, given what we pushed, where the branch is now, and what
/// Pages last built.
///
/// A function of three answers and nothing else, so the loop below is a
/// `match` over states that can be written down in a test rather than a
/// sequence of sleeps that can only be watched in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The tip is what Pages serves. Done.
    Served,
    /// Somebody pushed after us. Their run settles the branch, and asking for
    /// a build of *their* tree from here would only race them.
    NotOursAnyMore,
    /// Something is in flight, or the automatic build has not appeared yet.
    Wait,
    /// Nothing is in flight and the tip is not what was built: the deployment
    /// this exists for was refused, or never started.
    Ask,
}

pub fn step(pushed: &str, tip: &str, latest: Option<&Build>, waited: u32) -> Step {
    if tip != pushed {
        return Step::NotOursAnyMore;
    }
    match latest {
        // Ours, and finished one way or the other. An `errored` build of our
        // own tip is the collision: it is not going to become `built` on its
        // own, and only a new build will.
        Some(build) if build.commit == tip && !build.in_flight() => {
            if build.status == "built" {
                Step::Served
            } else {
                Step::Ask
            }
        }
        // Ours and still running, or somebody else's and still running: a
        // build in flight has to finish before another can be created at all,
        // so there is nothing to do but wait for it.
        Some(build) if build.in_flight() => Step::Wait,
        // Nothing running, and the newest build is of an older tree — or
        // there has never been one. Either the push's own build has not been
        // queued yet, which is worth a moment, or it was refused, which is
        // worth an ask.
        _ => {
            if waited < GRACE {
                Step::Wait
            } else {
                Step::Ask
            }
        }
    }
}

/// Wait for the tree we pushed to be the one Pages serves, asking for a build
/// if nothing else will.
///
/// `remote` is the URL to read the branch tip back from — the same one the
/// push went to, token and all, because this may be a private repository and
/// is certainly not a checkout by the time we get here.
pub fn settle(api: &Api, remote: &str, branch: &str, pushed: &str) -> Result<()> {
    let mut asks = 0;
    let mut waited = 0;
    for _ in 0..POLLS {
        let tip = tip_of(remote, branch)?;
        let latest = latest_build(api)?;
        match step(pushed, &tip, latest.as_ref(), waited) {
            Step::Served => {
                say!("Pages is serving {}", short(pushed));
                return Ok(());
            }
            Step::NotOursAnyMore => {
                say!(
                    "{branch} has moved on to {}; that run settles it",
                    short(&tip)
                );
                return Ok(());
            }
            Step::Wait => {
                waited += 1;
                sleep(EVERY);
            }
            Step::Ask => {
                if asks >= ASKS {
                    return Err(err!(
                        "asked Pages for a build of {} {ASKS} times and it is still not serving \
                         it; the branch is right and the deployment is not",
                        short(pushed)
                    ));
                }
                note!("Pages has not built {}; asking for a build", short(pushed));
                match api.post("pages/builds")? {
                    Posted::Queued => {
                        asks += 1;
                        waited = 0;
                    }
                    // One started between our reading and our asking, which
                    // is what we wanted to happen. Waiting for it is not an
                    // attempt spent.
                    Posted::AlreadyBusy => note!("one was already queued; waiting for it"),
                }
                sleep(EVERY);
            }
        }
    }
    Err(err!(
        "gave up waiting for Pages to serve {}: the branch carries it and the deployment does not",
        short(pushed)
    ))
}

/// Wait, briefly, for Pages to be idle before writing to the branch.
///
/// Not a guard — `settle` is what makes this correct, and two runs can pass
/// this at the same instant. It is a courtesy to the log: a push landing
/// while a deployment is in flight produces a red run of a workflow nobody
/// here wrote, and holding the push for the twenty seconds it takes turns
/// most of those red runs into no runs at all.
///
/// So running out of patience here is not an error. The push is what matters
/// and it goes ahead either way.
pub fn wait_for_quiet(api: &Api) {
    for _ in 0..POLLS {
        match latest_build(api) {
            Ok(Some(build)) if build.in_flight() => sleep(EVERY),
            // Idle, never built, or the question broke — none of which is a
            // reason to hold a push. A broken question will be an error in
            // `settle`, where it is one.
            _ => return,
        }
    }
    note!("Pages has been building for a while; pushing anyway");
}

/// What Pages last built, or `None` if it never has.
fn latest_build(api: &Api) -> Result<Option<Build>> {
    let Some(body) = api.get_if_there("pages/builds/latest")? else {
        return Ok(None);
    };
    // A build with no `status` or no `commit` is the API not having said, and
    // guessing either way here is guessing whether the site is up to date.
    let (Some(status), Some(commit)) = (
        json::string_at(&body, "status"),
        json::string_at(&body, "commit"),
    ) else {
        return Err(err!(
            "the API answered about the latest Pages build with no status or commit in it"
        ));
    };
    Ok(Some(Build { status, commit }))
}

/// Where the branch is now, read from the remote rather than from a checkout:
/// by this point the working tree the push came from is gone, and the
/// question is about the remote anyway.
///
/// The failure is written out by hand rather than taken from `read()`,
/// because `read()` names the command it ran and this one's argument is a URL
/// with a token in it. Actions masks its own secrets in a log and that is a
/// safety net, not a licence: this is also run by hand, and a token belongs
/// in an error message from nowhere.
fn tip_of(remote: &str, branch: &str) -> Result<String> {
    let git = Run::new("git")
        .args(["ls-remote", remote, &format!("refs/heads/{branch}")])
        .output()?;
    if !git.status.success() {
        return Err(err!(
            "could not read {branch} back from the remote: {}",
            scrubbed(&String::from_utf8_lossy(&git.stderr))
        ));
    }
    Ok(String::from_utf8_lossy(&git.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// Whatever git had to say, with any credentials in it taken out.
///
/// git writes most URLs back without their userinfo, and "most" is not the
/// standard for a token: one message that keeps it would put a push
/// credential in a log that outlives the run. Everything between a scheme and
/// the `@` that ends its userinfo goes.
fn scrubbed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + "://".len());
        out.push_str(before);
        // Only within the authority: an `@` after the next `/` or a space is
        // part of something else entirely.
        let authority = after
            .find(['/', ' ', '\t', '\n', '\'', '"'])
            .unwrap_or(after.len());
        match after[..authority].find('@') {
            Some(userinfo) => {
                out.push_str("<credentials>@");
                rest = &after[userinfo + 1..];
            }
            None => {
                out.push_str(&after[..authority]);
                rest = &after[authority..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// A commit, at the length a person reads.
fn short(sha: &str) -> &str {
    match sha.char_indices().nth(8) {
        Some((at, _)) => &sha[..at],
        None => sha,
    }
}

#[cfg(test)]
mod tests {
    use super::{Build, Step, scrubbed, short, step};

    fn build(status: &str, commit: &str) -> Build {
        Build {
            status: status.to_string(),
            commit: commit.to_string(),
        }
    }

    /// The case this module exists for: our push landed, the deployment it
    /// started was refused because another was in flight, and nothing else is
    /// ever going to build this tree.
    #[test]
    fn a_refused_deployment_is_asked_for_again() {
        assert_eq!(
            step("ours", "ours", Some(&build("errored", "ours")), 0),
            Step::Ask
        );
        // And once it has been built, that is the end of it.
        assert_eq!(
            step("ours", "ours", Some(&build("built", "ours")), 0),
            Step::Served
        );
    }

    /// A build in flight has to finish before another can be created at all,
    /// so neither ours nor anybody else's is a reason to ask for one.
    #[test]
    fn nothing_is_asked_for_while_something_is_building() {
        for status in ["queued", "building"] {
            assert_eq!(
                step("ours", "ours", Some(&build(status, "ours")), 99),
                Step::Wait
            );
            assert_eq!(
                step("ours", "ours", Some(&build(status, "theirs")), 99),
                Step::Wait
            );
        }
    }

    /// GitHub queues the automatic build a few seconds after the push. Asking
    /// before it appears spends an ask on a build that was already coming —
    /// so the wait is graced, and only patience running out is an ask.
    #[test]
    fn the_automatic_build_is_given_a_moment_to_appear() {
        let older = build("built", "the tree before ours");
        assert_eq!(step("ours", "ours", Some(&older), 0), Step::Wait);
        assert_eq!(step("ours", "ours", Some(&older), 11), Step::Wait);
        assert_eq!(step("ours", "ours", Some(&older), 12), Step::Ask);
        // A site that has never been built at all reads the same way.
        assert_eq!(step("ours", "ours", None, 0), Step::Wait);
        assert_eq!(step("ours", "ours", None, 12), Step::Ask);
    }

    /// A run whose push has been overtaken has nothing left to settle: a
    /// legacy build takes the tip as it finds it, so asking for one from here
    /// would only race the run that owns it.
    #[test]
    fn a_run_that_has_been_overtaken_stands_down() {
        for latest in [
            Some(build("errored", "ours")),
            Some(build("built", "ours")),
            None,
        ] {
            assert_eq!(
                step("ours", "theirs", latest.as_ref(), 99),
                Step::NotOursAnyMore
            );
        }
    }

    /// The remote is a URL with a push token in it, so anything git says
    /// about it is scrubbed before it reaches a log that outlives the run.
    #[test]
    fn a_token_never_reaches_the_log() {
        let said = scrubbed(
            "fatal: could not read Username for \
             'https://x-access-token:ghs_secret@github.com/oxidezap/client.git'",
        );
        assert!(!said.contains("ghs_secret"), "{said}");
        assert!(
            said.contains("<credentials>@github.com/oxidezap/client.git"),
            "{said}"
        );

        // A URL with nothing to hide survives intact, and so does ordinary
        // prose with an `@` in it.
        assert_eq!(
            scrubbed("remote: see https://github.com/oxidezap/client/pull/1"),
            "remote: see https://github.com/oxidezap/client/pull/1"
        );
        assert_eq!(scrubbed("ask me@example.com"), "ask me@example.com");
    }

    #[test]
    fn a_commit_is_shortened_for_reading() {
        assert_eq!(
            short("057b7ae1c0cf9c1fe49c8f233ef01ae1a0995df0"),
            "057b7ae1"
        );
        assert_eq!(short("abc"), "abc");
    }
}
