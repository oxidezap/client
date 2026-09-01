//! The repository's tasks, as one program.
//!
//! What used to live here was two shell scripts and two more written into
//! `$RUNNER_TEMP` by heredoc, plus the `run:` blocks that translated between
//! their exit codes. They were the two most intricate pieces of logic in the
//! tree with nothing testing them: a compare-and-swap against a git branch,
//! and a three-way "is this still wanted" whose one wrong reading — an
//! operational failure collapsing into "stand down" — is a deployment that
//! silently does not happen while the job reports success.
//!
//! So they are Rust, in a crate small enough to build in the job that needs
//! it. Nothing here is a rewrite of the *decisions*; the comments those
//! scripts carried came with them, because the decisions are the valuable
//! part and this is the same reasoning with a compiler and a test runner
//! under it.
//!
//!     cargo xtask web build
//!     cargo xtask web map [dist]
//!     cargo xtask bundle check [dir] [--relocatable]
//!     cargo xtask bundle size  [dir]
//!     cargo xtask pages where
//!     cargo xtask pages publish       --target <t> --ordinal <n> [--check <c>] [--settle]
//!     cargo xtask pages remove        --target <t> --ordinal <n> [--check <c>] [--settle]
//!     cargo xtask pages undo-if-closed --target <t> --ordinal <n> [--settle]

mod bundle;
mod check;
mod deploy;
mod json;
mod pages;
mod sourcemap;
mod util;
mod web;

use std::path::PathBuf;

use check::{Api, Precondition, State};
use pages::{Action, Job, Remote};
use util::{Result, append_github_file, env_or, need_env, repo_root};

/// Something the person running this wants to read. `println!` by another
/// name, kept as a macro so every task says things the same way and so the
/// reason the root's `print_stdout` deny does not apply here is stated once.
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { println!($($arg)*) };
}

/// The same, on the stream a runner shows in red.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = dispatch(&argv) {
        note!("xtask: {e}");
        std::process::exit(1);
    }
}

const USAGE: &str = "\
usage: cargo xtask <task>

  web build                      build the web front end into web/dist
  web map [dir]                  write a source map beside the module in it,
                                 out of the DWARF a `WEB_PROFILE=dwarf` build
                                 left in the module
  bundle check [dir] [--relocatable]
                                 check the bundle is complete, and — for the
                                 archive a release carries — that nothing in
                                 it is named from the origin root
  bundle size  [dir]             measure it, and say so in the step summary
  pages where                    work out where this run publishes to
  pages publish --target <t> --ordinal <n> [--check <c>] [--bundle <dir>] [--settle]
  pages remove  --target <t> --ordinal <n> [--check <c>] [--settle]
  pages undo-if-closed --target <t> --ordinal <n> [--settle]

  <c> is one of: always, still-current, now-closed, still-closed
  --settle waits for Pages to serve what was pushed, and asks for a build
           when the push's own deployment was refused";

fn dispatch(args: &[&str]) -> Result<()> {
    match args {
        ["web", "build"] => web::build(),
        ["web", "map", rest @ ..] => web::map(&dist_of(rest)),
        ["bundle", "check", rest @ ..] => {
            let (rest, relocatable) = relocatable_flag(rest)?;
            bundle::check(&dist_of(&rest), relocatable)
        }
        ["bundle", "size", rest @ ..] => bundle::size(&dist_of(rest)),
        ["pages", "where"] => where_this_goes(),
        ["pages", verb @ ("publish" | "remove"), rest @ ..] => {
            let action = if *verb == "publish" {
                Action::Publish
            } else {
                Action::Remove
            };
            pages::run(&job(action, rest)?, &Remote::from_env()?)
        }
        ["pages", "undo-if-closed", rest @ ..] => undo_if_closed(rest),
        ["help" | "--help" | "-h"] | [] => {
            say!("{USAGE}");
            Ok(())
        }
        other => Err(err!("unknown task `{}`\n\n{USAGE}", other.join(" "))),
    }
}

/// `--relocatable`, in either position, taken off the arguments so what is
/// left is the directory. A flag rather than a second task, because it is one
/// more question about the same bundle and the answer to the rest of them is
/// identical.
fn relocatable_flag<'a>(args: &[&'a str]) -> Result<(Vec<&'a str>, bool)> {
    let mut rest = Vec::new();
    let mut relocatable = false;
    for arg in args {
        if *arg == "--relocatable" {
            relocatable = true;
        } else if arg.starts_with("--") {
            return Err(err!("unexpected argument `{arg}`\n\n{USAGE}"));
        } else {
            rest.push(*arg);
        }
    }
    Ok((rest, relocatable))
}

fn dist_of(rest: &[&str]) -> PathBuf {
    match rest {
        [dir, ..] => PathBuf::from(dir),
        [] => repo_root().join("web").join("dist"),
    }
}

/// Flags in any order, because the call sites pass several and a positional
/// version silently ignored whichever came second.
fn job(action: Action, args: &[&str]) -> Result<Job> {
    let mut target = None;
    let mut ordinal = None;
    let mut check = None;
    let mut bundle = None;
    let mut settle = false;
    let mut rest = args;
    while let [flag, tail @ ..] = rest {
        let value = || -> Result<&str> {
            tail.first()
                .copied()
                .ok_or_else(|| err!("{flag} needs a value"))
        };
        match *flag {
            "--target" => {
                target = Some(value()?.to_string());
                rest = &tail[1..];
            }
            "--ordinal" => {
                let raw = value()?;
                ordinal = Some(
                    raw.trim()
                        .parse::<u64>()
                        .map_err(|_| err!("--ordinal wants a number, not `{raw}`"))?,
                );
                rest = &tail[1..];
            }
            "--check" => {
                check = Some(Precondition::parse(value()?)?);
                rest = &tail[1..];
            }
            "--bundle" => {
                bundle = Some(PathBuf::from(value()?));
                rest = &tail[1..];
            }
            // No value: a write to the branch either sees its deployment
            // through or it does not.
            "--settle" => {
                settle = true;
                rest = tail;
            }
            other => return Err(err!("unexpected argument `{other}`\n\n{USAGE}")),
        }
    }

    Ok(Job {
        action,
        target: target.ok_or_else(|| err!("--target is required"))?,
        // Without it there is no way to tell an older publish from a newer
        // one, which is the whole of what `.publish/` is for.
        ordinal: ordinal.ok_or_else(|| {
            err!("--ordinal is required: without it there is no way to tell an older publish from a newer one")
        })?,
        check: check.unwrap_or(Precondition::Always),
        settle,
        bundle: bundle.unwrap_or_else(|| PathBuf::from("bundle")),
    })
}

/// Where this run's bundle is served from, which the generated glue has to be
/// told or it fetches the wasm from the account root and gets a 404. A preview
/// lives one directory deeper than the site.
///
/// One command for both jobs that asked it, because they were the same
/// `if`/`else` written twice with one output key different.
fn where_this_goes() -> Result<()> {
    let repository = need_env("GITHUB_REPOSITORY")?;
    let (owner, repo) = repository
        .split_once('/')
        .ok_or_else(|| err!("GITHUB_REPOSITORY should be owner/repo, not `{repository}`"))?;

    let (public_url, target) = if env_or("GITHUB_EVENT_NAME", "") == "pull_request" {
        let n = need_env("PR_NUMBER")?;
        (format!("/{repo}/pr/{n}/"), format!("pr/{n}"))
    } else {
        (format!("/{repo}/"), ".".to_string())
    };
    let path = public_url.trim_start_matches('/');
    let url = format!("https://{owner}.github.io/{path}");

    say!("target={target} public_url={public_url} url={url}");
    append_github_file(
        "GITHUB_OUTPUT",
        &format!("public_url={public_url}\ntarget={target}\nurl={url}\n"),
    )
}

/// If the pull request closed while we were publishing, undo it.
///
/// The check runs before every attempt at the branch, and that still leaves
/// one gap it cannot close: a preview being published for the *first* time has
/// nothing on `gh-pages` yet, so the close job finds nothing to remove and
/// exits without touching the branch — which means it does not invalidate our
/// lease either, and the push that follows publishes a preview for a pull
/// request nobody will ever close again.
///
/// A lease cannot help there, because the two jobs are not both writing. So
/// this is a compensation rather than a guard: publish, ask again, and take it
/// back down if the answer changed. Either order converges — if the close job
/// runs after us it finds the preview and removes it, and if it ran before us
/// we remove it here.
fn undo_if_closed(args: &[&str]) -> Result<()> {
    let mut job = job(Action::Remove, args)?;
    // Re-asked per attempt, like every other write to this branch: the pull
    // request can be reopened while this removal is fetching, and the
    // `reopened` run's preview would then be what we delete.
    job.check = Precondition::NowClosed;

    // The same three-way reading the publish loop uses, because the same three
    // things can happen and only one of them means remove. An error is the
    // check *breaking* and stays an error: reading that as "not closed" is how
    // a closed pull request's preview stays published for good, since the
    // close event has already been spent and nothing comes back to clean it
    // up.
    let removed = match Api::from_env()?.state()? {
        // Still open on this head: the preview we just published is the right
        // one.
        State::Current => {
            say!("still open; the preview stays");
            false
        }
        // A newer run of this same pull request published while we worked, so
        // what is on the branch is newer than what we had — removing it would
        // delete a preview that is wanted.
        State::Superseded => {
            say!("superseded; a newer run owns the preview");
            false
        }
        // Ours to undo.
        State::Closed => {
            note!("closed while publishing; removing {} again", job.target);
            pages::run(&job, &Remote::from_env()?)?;
            true
        }
    };

    append_github_file("GITHUB_OUTPUT", &format!("removed={removed}\n"))
}
