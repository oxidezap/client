//! What the build produced, checked and measured.
//!
//! Both of these were `run:` blocks of shell in the Pages workflow, and both
//! were the kind that is hard to read and impossible to test: a `grep -oE`
//! into a `sed -E` into a `basename` loop, and an `awk` invoked twice to
//! divide by 1048576. The check in particular is load-bearing — it is what
//! stands between a rewritten `index.html` and a blank page — so it is worth
//! being something with tests under it.

use std::fs;
use std::path::Path;

use crate::util::{Result, Run, append_github_file};
use crate::{err, say};

/// Proof the artifact is the one the page will ask for: trunk rewrites
/// `index.html` to point at hashed file names, and a mismatch is a blank page
/// rather than a failed build.
pub fn check(dist: &Path) -> Result<()> {
    let index = dist.join("index.html");
    if !index.is_file() {
        return Err(err!("no index.html in {}", dist.display()));
    }
    if !dist.join("coi-serviceworker.js").is_file() {
        return Err(err!(
            "the service worker is missing: the page will load without \
             cross-origin isolation and the window will have no executor"
        ));
    }

    let html =
        fs::read_to_string(&index).map_err(|e| err!("could not read {}: {e}", index.display()))?;
    for asset in local_assets(&html) {
        // Trunk emits everything flat, so the reference's last segment is the
        // file — which is also what makes a query string or a fragment on it
        // harmless to strip.
        let name = basename(&asset);
        if !dist.join(name).is_file() {
            return Err(err!(
                "index.html references {asset}, which is not in {}",
                dist.display()
            ));
        }
    }

    say!("bundle contents:");
    let mut names: Vec<_> = fs::read_dir(dist)?
        .filter_map(|e| e.ok())
        .map(|e| {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            (e.file_name().to_string_lossy().into_owned(), size)
        })
        .collect();
    names.sort();
    for (name, size) in names {
        say!("  {name}  {size} bytes");
    }
    Ok(())
}

/// The number a regression shows up in. `check` proves the bundle is
/// *complete*; nothing proved it was not twice the size it was last week, and
/// a module a visitor downloads before the first pixel is the one measurement
/// this build can take for free.
///
/// Printed to the step summary rather than asserted against a threshold: a
/// build that fails because a legitimate feature cost 200 KB teaches people to
/// raise the number, and the useful signal is the trend beside the diff that
/// caused it.
pub fn size(dist: &Path) -> Result<()> {
    let wasm = fs::read_dir(dist)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_bg.wasm"))
        })
        .ok_or_else(|| err!("no *_bg.wasm in {}", dist.display()))?;

    let bytes = fs::metadata(&wasm)?.len();
    // What Pages actually serves. Through `gzip`, for the reason `curl` is
    // still a program: a deflate implementation is a dependency, and this
    // directory has none.
    let gzipped = Run::new("gzip").args(["-9", "-c"]).arg(&wasm).output()?;
    if !gzipped.status.success() {
        return Err(err!("could not gzip {}", wasm.display()));
    }
    let gzip = gzipped.stdout.len() as u64;
    let shown = wasm.to_string_lossy();
    let name = basename(&shown);

    append_github_file(
        "GITHUB_STEP_SUMMARY",
        &format!(
            "### Bundle size\n\n\
             | | bytes | MiB |\n\
             |---|---:|---:|\n\
             | `{name}` | {bytes} | {} |\n\
             | the same, gzipped (what Pages serves) | {gzip} | {} |\n",
            mib(bytes),
            mib(gzip),
        ),
    )?;
    say!("wasm: {bytes} bytes, {gzip} gzipped");
    Ok(())
}

fn mib(bytes: u64) -> String {
    // Two decimals, without pulling in a formatter that rounds differently
    // from the `awk` this replaced.
    format!("{:.2}", bytes as f64 / 1_048_576.0)
}

/// Every `href="…"` and `src="…"` in the document that names a file in the
/// bundle rather than somewhere else.
fn local_assets(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    for attr in ["href=\"", "src=\""] {
        let mut rest = html;
        while let Some(at) = rest.find(attr) {
            rest = &rest[at + attr.len()..];
            let Some(end) = rest.find('"') else { break };
            let value = &rest[..end];
            rest = &rest[end + 1..];
            if value.is_empty() || value.starts_with('#') || value.starts_with("data:") {
                continue;
            }
            // Absolute and protocol-relative references are somebody else's
            // to serve.
            if value.starts_with("http://")
                || value.starts_with("https://")
                || value.starts_with("//")
            {
                continue;
            }
            found.push(value.to_string());
        }
    }
    found
}

fn basename(path: &str) -> &str {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::{basename, local_assets, mib};

    #[test]
    fn only_the_bundle_s_own_references_are_collected() {
        let html = r##"
            <link rel="preload" href="/client/oxidezap-abc123_bg.wasm" as="fetch">
            <link rel="stylesheet" href="https://fonts.example/x.css">
            <script src="//cdn.example/y.js"></script>
            <script src="/client/oxidezap-abc123.js"></script>
            <a href="#top">top</a>
            <img src="data:image/png;base64,AAAA">
        "##;
        let mut assets = local_assets(html);
        assets.sort();
        assert_eq!(
            assets,
            vec![
                "/client/oxidezap-abc123.js".to_string(),
                "/client/oxidezap-abc123_bg.wasm".to_string(),
            ]
        );
    }

    #[test]
    fn a_reference_resolves_to_the_flat_file_trunk_emitted() {
        assert_eq!(
            basename("/client/pr/12/oxidezap-abc_bg.wasm"),
            "oxidezap-abc_bg.wasm"
        );
        assert_eq!(basename("coi-serviceworker.js?v=2"), "coi-serviceworker.js");
        assert_eq!(basename("index.html#top"), "index.html");
    }

    #[test]
    fn mebibytes_are_what_the_awk_printed() {
        assert_eq!(mib(1_048_576), "1.00");
        assert_eq!(mib(29_825_238), "28.44");
        assert_eq!(mib(0), "0.00");
    }
}
