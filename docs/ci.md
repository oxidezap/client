# CI & dependencies

> **Every figure below is a measurement, each read off one job's own upload log
> at one commit.** They are evidence for the rules, not facts to plan against —
> and the cache ceiling is GitHub's number, not ours, so check the current one
> before doing arithmetic with it. `.github/workflows/` is the authority for
> what is actually cached and by which job; the rules here are what survive a
> re-measurement.

## The library dependency

Every `whatsapp-rust` crate resolves from one git source on one branch, so
`cargo update` moves them together and no two can land on incompatible
revisions. `Cargo.toml` is the list — some are declared directly and others
arrive transitively, and the profile table names more of them than the
dependency table does, so count them there rather than here. Never pin them individually by `rev`: the resulting mismatch surfaces
as "expected `Jid`, found `Jid`" and reads like a compiler bug.

Because profile settings only apply from the workspace root, the per-package
`opt-level` sweep in the library's own manifest is *not* inherited, so the release
profile here repeats it deliberately.

## What CI actually costs

A repository gets 10 GB of Actions cache, and GitHub evicts the least recently
used entry to stay under it. That number, not any compiler setting, is what
decides how long a pull request waits: a job that restores its cache spends
under a minute on the download and then compiles what changed, and the same
job that finds nothing compiles the world. The Windows `Check` job has been
observed at 3m49 with a cache and 12m26 without it, twenty minutes apart.
With every entry restoring, a pull request's whole run lands around 6m20
against the 12m45 it took when one of them was always missing.

So the budget is a shared resource with a fixed size, and every `save-if` in
these workflows is a claim on it. Two rules follow, and both are already in the
files:

- **Only `main` writes — with one exception worth knowing before you do the
  arithmetic.** A pull request restores and never saves, or every branch would
  push out the one entry every other branch restores from. The exception is
  `web-bundle.yml`: its rust-cache step carries a `key` but no `save-if`, and
  `release.yml` calls it on a tag, so a release writes an entry too. Count it
  when sizing the budget, or release builds evict the `main` entries this
  policy exists to protect.
- **A job that is nobody's critical path does not cache a target directory.**
  The five entries the other workflows write come to 8 GB of the 10 —
  2.17 GB for the Linux `Check`, 2.07 for Windows, 1.64 for macOS, 0.87 for
  MSRV, 1.14 for `pages-wasm`, each read off its own upload log — and
  `build.yml` wrote three more on top, one release target directory per
  platform under fat LTO. There is no version of that which fits, so it keeps
  the registry and the git checkouts (`cache-targets: false`) and recompiles
  the rest. Its Windows job was spending 9m15 of a 17m45 run moving that cache
  around (2m05 down, 7m10 up), which is the shape of the thing being given up.

The other half is how big an entry is, because the download and the upload are
themselves a minute each. What rust-cache stores is the *dependencies* — it
prunes the workspace's own artifacts before saving — so the lever is what a
dependency compiles to rather than what our crates compile to. Dependencies
carry no debug information (`[profile.dev.package."*"]`, and `build-override`
for the host-compiled half of them, which is where the proc macros are), which
is why they can: nobody sets a breakpoint in diesel, and `panic::Location` is
compiled in regardless, so panics still name their file and line.

What that is worth turned out to be a question about the platform, and the
first version of this paragraph got it wrong by answering it from one machine.
Measured off each job's own upload log, before and after:

  Check (Linux)     2.17 GB -> 1.72 GB   -21%
  Check (Windows)   2.07 GB -> 1.95 GB    -6%
  Check (macOS)     1.64 GB -> 1.58 GB    -4%

The claim here used to be "a third off", extrapolated from a local Linux
measurement that also excluded the GUI crate. It holds on Linux and nowhere
else, which in hindsight is what should have been expected: DWARF is the
format on one of these three platforms, and Windows puts debug information in
PDBs while macOS leaves it in the object files. A number measured on one
target is a number about that target.

It is still not a trade against build time — emitting and linking debug
information is work, so the cold local build got 12% faster as well, and the
manifest carries that table.

Which is also why every one of those caches keys on the root manifest's hash.
rust-cache's automatic key hashes `Cargo.lock`, `.cargo/config.toml` and each
*member* crate's manifest — not the root one, where the profiles live. A
profile decides every dependency's fingerprint, so editing one leaves the key
identical while making every cached artifact useless, and the failure is
silent in the worst way: the restore reports a full match, cargo rebuilds all
of it anyway, and rust-cache then declines to save ("Cache up-to-date."), so
the stale entry is never replaced. That is not a slow first run; it is a cache
that can no longer be refreshed. It was measured on `main`, where the Windows
`Check` job went from 3m49 warm to 11m17 and stayed there until the key
learned to name the file that had changed. `build.yml` is the one exception,
and for the reason the rule exists: it stores `~/.cargo` alone, and a profile
cannot make a downloaded `.crate` stale.

Raising the 10 GB limit is possible on a paid plan and would be another answer
to the same problem. Nothing here needs it yet.


## The one workflow this repository does not own

`pages.yml` writes to `gh-pages`; it does not deploy. Every push to that
branch starts a run of **`pages build and deployment`**, which GitHub
generates, which is not a file in this tree, and which cannot be configured
from one. That run is where a Pages site is actually served from, and it is
the only piece of this pipeline nobody here can edit.

Its one failure mode matters because the way `pages.yml` is designed makes it
common. The deployment API refuses to create a deployment while another one is
in flight:

    Deployment request failed for <sha> due to in progress deployment.
    Please cancel <other> first or wait for it to complete.

Two writers landing seconds apart is the ordinary case, not a rare one — a
pull request closing takes its preview down while the merge that closed it
publishes the site — and it is a *consequence* of the lease in `xtask pages`,
which is what makes both writes land rather than one silently replacing the
other. The refused one then leaves the branch carrying a tree that nothing
will serve: Pages deploys on a push, and the push has already happened. For a
preview take-down that is the end of the line, because a pull request closes
once and the preview stays live.

Re-running the failed run is the wrong instinct and the reason the error
people actually see is a different one. The built-in workflow's build job
uploads its artifact again into the same run, and the deploy job then refuses
with `Multiple artifacts named "github-pages" were unexpectedly found for this
workflow run. Artifact count is 2.` — which reads like an artifact bug and is
a collision one step underneath.

So the push is not the end of the publisher's job. `xtask pages … --settle`
(`xtask/src/deploy.rs`) asks Pages what it last built, and asks it for a build
when the branch tip is not that — a convergence rather than a lock, for the
same reason the branch write is one: a legacy Pages build takes the tip as it
finds it, so any run that notices can fix it and a run that has been overtaken
can stand down. It needs `pages: write` on the job, which is why `publish` and
`take-down` carry it.
