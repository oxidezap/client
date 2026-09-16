// Cross-origin isolation on a host that will not set headers for us.
//
// `gpui_web` runs its background executor on real workers, and workers share
// memory through a `SharedArrayBuffer`. A browser hands one out only to a
// page that is cross-origin isolated, which needs two response headers:
//
//     Cross-Origin-Opener-Policy: same-origin
//     Cross-Origin-Embedder-Policy: require-corp
//
// GitHub Pages serves static files and offers no way to add them. A service
// worker can, because it answers requests for its own scope — so the first
// load registers this and reloads once, and every navigation after that is
// served through here and is already isolated.
//
// *Once* is load-bearing rather than descriptive. A reload is the only way
// this takes effect and it is also the only thing here that can run away: a
// page that reloads whenever a worker takes control reloads on every update,
// and a worker that skips waiting and claims makes every update a takeover.
// The page half below bounds it — once per document, once per tab — and says
// what it knows instead of navigating again.
//
// *Navigations*, and worker scripts: those are the two responses the headers
// mean anything on, and answering the rest of them costs the page a second
// download of everything it preloaded. See the fetch handler below.
//
// This is the only hand-written JavaScript in the build; everything else on
// the page is Rust through `wasm-bindgen`. It is here because the alternative
// is a header we have no way to send, not because anything below is easier in
// JavaScript than in Rust.
//
// The technique is the widely used `coi-serviceworker` pattern.

if (typeof window === "undefined") {
    // ---- Running as the service worker ----------------------------------
    self.addEventListener("install", () => self.skipWaiting());
    self.addEventListener("activate", (event) =>
        event.waitUntil(self.clients.claim()),
    );

    // What the two headers are *for* is a context, not a byte stream:
    // COOP and COEP describe a document or a worker, and a page's ordinary
    // subresources are governed by `Cross-Origin-Resource-Policy` instead —
    // which same-origin bytes pass with no header at all. So rewriting the
    // headers of the module, the glue and the icons changed nothing about
    // isolation, and it was not free: a request a service worker answers is
    // a different "world" from the one `<link rel="preload">` fetched in,
    // so the browser refuses to match the two and fetches the ~30 MB module
    // a second time —
    //
    //     A preload for '…_bg.wasm' is found, but is not used because it is
    //     a cross-world service worker resource mismatch.
    //
    // Passing a request through (returning without `respondWith`) leaves it
    // in the page's own world, where the preload is waiting for it.
    const ISOLATED = new Set([
        // A worker script's own response carries the policy the worker is
        // created under, so this one is not optional: a dedicated worker
        // fetched over http(s) into a `require-corp` page needs the header
        // or it is refused. (`gpui_web`'s executor threads start from a
        // `blob:` URL, which inherits the page's policy and never reaches
        // here — but a URL-loaded worker is one dependency away.)
        "worker",
        "sharedworker",
        // And a nested document, which is a context of its own.
        "iframe",
        "frame",
        "document",
    ]);

    self.addEventListener("fetch", (event) => {
        const request = event.request;
        // A navigation preload response cannot have headers rewritten, and a
        // range request must be passed through untouched or media seeking
        // breaks.
        if (request.cache === "only-if-cached" && request.mode !== "same-origin") {
            return;
        }

        // Everything else is a subresource: the page's own world serves it,
        // out of the preload if one is pending.
        if (request.mode !== "navigate" && !ISOLATED.has(request.destination)) {
            return;
        }

        event.respondWith(
            fetch(request)
                .then((response) => {
                    if (response.status === 0) {
                        // An opaque response: nothing to rewrite, and copying
                        // it would throw.
                        return response;
                    }
                    const headers = new Headers(response.headers);
                    headers.set("Cross-Origin-Embedder-Policy", "require-corp");
                    headers.set("Cross-Origin-Opener-Policy", "same-origin");
                    return new Response(response.body, {
                        status: response.status,
                        statusText: response.statusText,
                        headers,
                    });
                })
                .catch((error) => {
                    console.error("coi-serviceworker:", error);
                    throw error;
                }),
        );
    });
} else {
    // ---- Running on the page --------------------------------------------
    // A reload is the only way this takes effect, and it is also the only way
    // it can fail catastrophically: a page that reloads to become isolated and
    // is not isolated when it comes back will reload again, and there is
    // nothing in the second attempt that was not in the first. So the reload
    // is bounded twice over — once per document, and once per tab — and what
    // is past the bound is a console error rather than another navigation.
    const RELOADED = "coi-serviceworker:reloaded";

    // `sessionStorage` is per tab and survives a reload, which is exactly the
    // scope the bound wants. A browser may refuse it (a privacy mode, a
    // blocked storage context), and a refusal must not be read as "not yet
    // reloaded" — that is the reading that loops — so it counts as spent.
    const alreadyReloaded = () => {
        try {
            return window.sessionStorage.getItem(RELOADED) !== null;
        } catch (error) {
            console.warn("coi-serviceworker: no session storage:", error);
            return true;
        }
    };

    if (window.crossOriginIsolated) {
        // Isolated: either this worker is serving us or the host set the
        // headers itself. Spend the mark, so that a later load which is
        // somehow not isolated — the registration removed by hand, a browser
        // that dropped it — still gets its one attempt.
        try {
            window.sessionStorage.removeItem(RELOADED);
        } catch {
            // Nothing was stored, so there is nothing to clear.
        }
    } else if (!window.isSecureContext) {
        // Service workers need HTTPS (or localhost). Say so rather than
        // failing later with "SharedArrayBuffer is not defined".
        console.error(
            "coi-serviceworker: this page is not a secure context, so it " +
                "cannot be cross-origin isolated. Serve it over HTTPS or " +
                "from localhost.",
        );
    } else if (!navigator.serviceWorker) {
        console.error(
            "coi-serviceworker: this browser has no service workers, so " +
                "the window cannot start its background executor.",
        );
    } else if (alreadyReloaded()) {
        // We have been here already in this tab, and came back without the
        // headers. Reloading again would only ask the same question, so the
        // loop stops here and says what it knows: everything below needs a
        // `SharedArrayBuffer`, and there will not be one.
        console.error(
            "coi-serviceworker: this page reloaded to pick up cross-origin " +
                "isolation and is still not isolated. The window will not " +
                "start. Check that the service worker is controlling this " +
                "page (Application → Service Workers) and that nothing " +
                "is stripping COOP/COEP from its responses.",
        );
    } else {
        // One reload, whichever of the two paths below asks for it first. A
        // `location.reload()` does not stop this script — the document is
        // torn down at the browser's convenience — so an unguarded call is a
        // call that can be made twice, and the second one cancels the
        // navigation the first one started. That is the shape the request
        // log showed: dozens of cancelled documents, tens of milliseconds
        // apart, none of them ever finishing.
        let reloading = false;
        const reloadOnce = () => {
            if (reloading) {
                return;
            }
            reloading = true;
            try {
                window.sessionStorage.setItem(RELOADED, "1");
            } catch (error) {
                // Then the bound above is gone and this is the only reload
                // there is. Which is the safe direction: it has been taken.
                console.warn("coi-serviceworker: no session storage:", error);
            }
            window.location.reload();
        };

        // Reload when the worker is actually *controlling* the page, not when
        // it starts installing. `updatefound` fires at the start of an
        // install, and a reload then lands on another uncontrolled navigation
        // which sees the same installing registration, gets no new event, and
        // sits there without a SharedArrayBuffer until somebody reloads by
        // hand. `controllerchange` is the event that means "from now on,
        // responses come through the worker".
        //
        // It fires for a *replacement* too, though, and that is the other
        // half of the loop: every navigation that finds a new script installs
        // a new worker, which skips waiting, activates and claims — so a page
        // that reloads on every `controllerchange` reloads on every update,
        // and DevTools' "Update on reload" makes every navigation an update.
        // The version counter climbing into the thousands is what that looks
        // like from the Application panel. This listener is registered only
        // on a page that is *not* isolated, so a replacement under a working
        // page is what it always should have been: nothing.
        navigator.serviceWorker.addEventListener("controllerchange", reloadOnce);
        navigator.serviceWorker
            .register(window.document.currentScript.src)
            .then((registration) => {
                // Already active but not yet controlling this page — the
                // navigation that registered it is never controlled by it,
                // and no `controllerchange` is coming for us.
                if (registration.active && !navigator.serviceWorker.controller) {
                    reloadOnce();
                }
            })
            .catch((error) =>
                console.error("coi-serviceworker: registration failed:", error),
            );
    }
}
