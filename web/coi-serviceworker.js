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
// load registers this and reloads once, and every load after that is served
// through here and is already isolated.
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

    self.addEventListener("fetch", (event) => {
        const request = event.request;
        // A navigation preload response cannot have headers rewritten, and a
        // range request must be passed through untouched or media seeking
        // breaks.
        if (request.cache === "only-if-cached" && request.mode !== "same-origin") {
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
    // Already isolated: either the worker is serving us, or the host set the
    // headers itself. Nothing to do.
    if (!window.crossOriginIsolated) {
        if (!window.isSecureContext) {
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
        } else {
            navigator.serviceWorker
                .register(window.document.currentScript.src)
                .then((registration) => {
                    // One reload, and only once the worker is in charge:
                    // reloading before it has claimed the page would loop.
                    registration.addEventListener("updatefound", () =>
                        window.location.reload(),
                    );
                    if (registration.active && !navigator.serviceWorker.controller) {
                        window.location.reload();
                    }
                })
                .catch((error) =>
                    console.error("coi-serviceworker: registration failed:", error),
                );
        }
    }
}
