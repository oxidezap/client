//! The browser half, run in a browser.
//!
//! Everything in the parent module is `web_sys` against a real OPFS, a real
//! `LockManager` and a real `localStorage`. None of it can be reached by
//! `cargo test` on the host, and the cost of that gap was not theoretical:
//! `entries` cast its iterator step with `dyn_into::<js_sys::IteratorNext>()`,
//! which can never succeed — the type has no `is_type_of`, so wasm-bindgen
//! emits `instanceof IteratorNext` against a global no browser defines, and
//! the shim's own `try`/`catch` turns the `ReferenceError` into `false`. Every
//! listing therefore failed, which meant installing failed, loading found
//! nothing and removing found nothing. Seven rounds of review passed over it
//! because reading the line is not what catches this; running it is.
//!
//! ```bash
//! # Chromium and its driver are what the runner needs; `RUSTFLAGS` is reset
//! # for the reason `examples/` resets it — the root's wasm flags are the web
//! # *front end's*, and a shared memory here would need headers this runner
//! # does not serve. The one flag that has to stay is the Web Locks cfg.
//! CHROMEDRIVER=$(which chromedriver) \
//! RUSTFLAGS='--cfg web_sys_unstable_apis' \
//! CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
//!   cargo test -p oxidezap-daemon --lib --target wasm32-unknown-unknown
//! ```
//!
//! The driver has to match the browser's major version, and it finds the
//! browser on the usual paths — where it is somewhere else, or where a
//! container needs `--no-sandbox`, a `webdriver.json` beside this crate's
//! manifest is what says so:
//!
//! ```json
//! { "goog:chromeOptions": { "binary": "/path/to/chrome",
//!                           "args": ["--headless=new", "--no-sandbox"] } }
//! ```
//!
//! These share one origin, so they share one plugin folder. Each clears what
//! it put there rather than assuming an empty one: a browser keeps OPFS for
//! the length of the session, and a test that left a file behind would be a
//! test the next one silently disagreed with.

use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

use super::{entries, folder, install, installed, names, uninstall};

wasm_bindgen_test_configure!(run_in_browser);

/// The smallest module a wasm host will parse: the magic and the version.
///
/// Enough for everything here, which is about the folder rather than about
/// what is in it — the host's own tests already cover loading.
const MODULE: &[u8] = b"\0asm\x01\0\0\0";

/// Take everything out of the folder, whatever a previous test left.
async fn empty_the_folder() {
    for id in names().await.expect("the folder can be listed") {
        uninstall(&id).await.expect("and emptied");
    }
}

/// Listing an empty folder answers an empty list.
///
/// The regression test, and it is deliberately the smallest one: this is the
/// call every other path goes through, and it answered `Err` with the
/// iterator's own `{done: true}` inside it — which reached the window as
/// "cannot install a plugin: JsValue(Object({"done":true}))", an error message
/// naming no file and no reason.
#[wasm_bindgen_test]
async fn an_empty_folder_lists_as_empty() {
    empty_the_folder().await;
    let dir = folder(true)
        .await
        .expect("this origin has a filesystem")
        .expect("and the folder was created");
    assert_eq!(
        entries(&dir).await.expect("an empty folder still lists"),
        Vec::<String>::new()
    );
}

/// And a folder with something in it answers what is in it.
///
/// The other half of the same bug: the first step of a non-empty listing is
/// `{done: false, value: "…"}`, which failed the same cast for the same
/// reason. One test would have caught it; two say which end broke.
#[wasm_bindgen_test]
async fn a_folder_lists_what_was_put_in_it() {
    empty_the_folder().await;
    install("listed.wasm", MODULE.to_vec())
        .await
        .expect("a module is installed");
    let dir = folder(false)
        .await
        .expect("the folder is readable")
        .expect("and it is there");
    assert_eq!(
        entries(&dir).await.expect("it lists"),
        vec!["listed.wasm".to_owned()]
    );
    empty_the_folder().await;
}

/// The whole gesture, in the order somebody performs it.
///
/// Installing, seeing it listed, having it handed to the host, and removing
/// it. Written as one test rather than four because what is being pinned is
/// that the steps agree with each other — the id `names` reports is the one
/// `uninstall` takes, and the bytes `installed` hands over are the ones that
/// went in.
#[wasm_bindgen_test]
async fn a_plugin_can_be_installed_listed_loaded_and_removed() {
    empty_the_folder().await;

    let id = install("round-trip.wasm", MODULE.to_vec())
        .await
        .expect("it installs");
    assert_eq!(id, "round-trip");
    assert_eq!(names().await.expect("it is listed"), vec!["round-trip"]);

    let modules = installed().await;
    assert_eq!(modules.len(), 1, "the host is handed exactly one module");
    assert_eq!(modules[0].id, "round-trip");
    let opened = (modules.into_iter().next().expect("the one module").open)()
        .expect("its bytes are readable");
    assert_eq!(opened, MODULE, "and they are the bytes that went in");

    uninstall("round-trip").await.expect("it is removed");
    assert!(
        names().await.expect("the folder still lists").is_empty(),
        "and the folder is empty again"
    );
}

/// A second install of the same id replaces rather than duplicates.
///
/// `install` writes `{id}.wasm` whatever the chosen file was called, so the
/// folder holds one entry per id; this is what makes updating a plugin an
/// update rather than a folder that grows.
#[wasm_bindgen_test]
async fn reinstalling_replaces() {
    empty_the_folder().await;
    install("twice.wasm", MODULE.to_vec())
        .await
        .expect("installed once");
    let longer = [MODULE, b"\0\0\0\0"].concat();
    install("twice.wasm", longer.clone())
        .await
        .expect("installed again");
    assert_eq!(names().await.expect("listed"), vec!["twice"]);
    let modules = installed().await;
    assert_eq!(modules.len(), 1);
    let opened = (modules.into_iter().next().expect("the one module").open)().expect("readable");
    assert_eq!(opened, longer, "the second write is what is there");
    empty_the_folder().await;
}

/// Removing something that is not there is not an error.
///
/// What a second press of Remove does, once the first has taken the file
/// away: the control outlives the file by design, so this has to answer `Ok`
/// rather than the browser's `NotFoundError`.
#[wasm_bindgen_test]
async fn removing_what_is_gone_is_not_an_error() {
    empty_the_folder().await;
    uninstall("never-installed")
        .await
        .expect("removing nothing succeeds");
}

/// A name this host cannot make an id out of is refused before anything is
/// written.
#[wasm_bindgen_test]
async fn a_name_that_is_not_an_id_is_refused() {
    assert!(install("../escape.wasm", MODULE.to_vec()).await.is_err());
    assert!(install("notwasm.txt", MODULE.to_vec()).await.is_err());
}
