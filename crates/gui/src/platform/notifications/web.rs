//! Browser notification backend. The page-level constructor is not
//! available in every browser, so the isolation service worker can post
//! and return an activation as a fallback.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::task::{Poll, Waker};

thread_local! {
    /// The banners still up, on the page's main thread — the only thread
    /// that ever shows a notification or receives its click. A `static`
    /// is out of reach here: `Notification` is a JS object and neither
    /// `Send` nor `Sync` under the shared-memory build. GPUI application
    /// callbacks and DOM events both run on this thread, so the map is
    /// never touched from a worker; the cross-thread half of the design
    /// is the tag queue below, which carries only strings.
    static LIVE: RefCell<HashMap<String, LiveNotification>> = RefCell::new(HashMap::new());
    /// Kept alive while the page listens for worker notification clicks.
    static WORKER_CLICKS: RefCell<Option<Closure<dyn FnMut(web_sys::MessageEvent)>>> =
        const { RefCell::new(None) };
}

use futures_lite::future::poll_fn;
use portable_atomic::{AtomicU64, Ordering};
use wasm_bindgen::prelude::*;
use web_sys::{Notification, NotificationOptions, NotificationPermission};

/// A banner the browser is still showing, kept alive for exactly as long.
///
/// Dropping the Rust wrapper lets the JS object be collected, which would
/// take its click handler with it, so the live set owns the notification
/// and both its closures. Replacing the entry for a tag drops the
/// previous set — the same moment the browser replaces the banner — and
/// revokes its icon URL, while a dismissed banner removes itself through
/// its close handler; either way nothing outlives its banner.
struct LiveNotification {
    id: u64,
    notification: Notification,
    _onclick: Closure<dyn FnMut()>,
    _onclose: Closure<dyn FnMut()>,
    icon_url: Option<String>,
}

/// Clicked tags waiting for the pump task, and the waker to end its wait.
///
/// One lock for both, taken once per operation and never nested: the
/// click callback and the pump poll run on different threads, and two
/// locks taken in opposite orders would be a circular wait between the
/// browser thread and the pump.
struct ActivationQueue {
    tags: VecDeque<String>,
    waker: Option<Waker>,
}

static ACTIVATIONS: OnceLock<Mutex<ActivationQueue>> = OnceLock::new();
static NEXT_LIVE_ID: OnceLock<Mutex<u64>> = OnceLock::new();
static ACCOUNT_EPOCH: AtomicU64 = AtomicU64::new(0);

fn activations() -> &'static Mutex<ActivationQueue> {
    ACTIVATIONS.get_or_init(|| {
        Mutex::new(ActivationQueue {
            tags: VecDeque::new(),
            waker: None,
        })
    })
}

fn next_live_id() -> u64 {
    NEXT_LIVE_ID
        .get_or_init(|| Mutex::new(1))
        .lock()
        .map(|mut next| {
            let id = *next;
            *next = next.wrapping_add(1);
            id
        })
        .unwrap_or(0)
}

/// Whether the Notification constructor exists in this context. Outside a
/// secure context the binding throws, so check before touching it and
/// degrade to silence rather than a panic.
fn notifications_available() -> bool {
    js_sys::Reflect::has(&js_sys::global(), &JsValue::from_str("Notification")).unwrap_or(false)
}

pub(super) fn request_gesture_authorization() {
    request_permission_for_prompt();
}

pub(super) fn request_authorization() {
    watch_worker_clicks();
    // Best effort: where the browser wants transient activation for the
    // prompt, this startup ask is ignored and the post-time ask below is
    // the one that counts — a message arriving while the user is in the
    // page — so an early grant here only ever saves that later round.
    request_permission_for_prompt();
}

/// Whether this tab is the one that should post for an incoming message.
///
/// Asked per post rather than settled once, because the arrangement can
/// change under the tab: a follower is promoted when its leader goes.
fn this_tab_should_post() -> bool {
    match oxidezap_ipc::web::named_daemon() {
        oxidezap_ipc::web::NamedDaemon::Named(_) => true,
        _ => crate::session::this_tab_holds_the_account(),
    }
}

/// Ask for notification permission where the browser will honour the ask.
///
/// Fire and forget: the promise settles after the user decides, and every
/// post re-reads the permission, so nothing here waits. A prompt needs
/// transient user activation on the browsers that gate it, so outside a
/// gesture this is a no-op and the call sites are the gesture-adjacent
/// moments: startup, a post attempted mid-interaction, and a banner click.
fn request_permission_for_prompt() {
    if !notifications_available() {
        return;
    }
    if Notification::permission() != NotificationPermission::Default {
        return;
    }
    // Absent on browsers without the concept, where the answer is yes —
    // a browser that never heard of activation does not gate prompts on
    // it either. Same shape as `platform::download`'s check.
    let engaged = web_sys::window().is_some_and(|window| {
        let activation = window.navigator().user_activation();
        activation.is_undefined() || activation.is_active()
    });
    if engaged {
        let _ = Notification::request_permission();
    }
}

pub(super) fn show_notification_with_avatar(
    tag: &str,
    title: &str,
    body: &str,
    avatar: impl Fn() -> Option<std::sync::Arc<Vec<u8>>> + Send + Sync + 'static,
) -> bool {
    if !notifications_available() {
        log::warn!("notification unavailable: this browser has no Notification API");
        return false;
    }
    // One banner per message, not per tab: every open tab receives the
    // same broadcast, so only the tab running the account posts — with
    // `renotify` on, a follower echoing the same tag would sound the
    // alert a second time for a banner only one tab keeps. Same
    // arrangement check as `capabilities::calls_belong_to_another_tab`:
    // a page onto an external daemon is its own window and posts, a
    // follower stays silent and leaves it to the leader.
    if !this_tab_should_post() {
        log::debug!("notification skipped: this tab does not own the account");
        return false;
    }
    if Notification::permission() == NotificationPermission::Default {
        // Opportunistic, not relied on: an arriving message carries no
        // activation of its own, so this prompts only when it lands
        // inside another gesture's window. The real ask is the
        // gesture-driven one in `select_chat`; the banner itself still
        // waits for the grant, and the next message finds the permission
        // settled.
        request_permission_for_prompt();
        return false;
    }
    if Notification::permission() != NotificationPermission::Granted {
        log::info!("notification suppressed: permission not granted");
        return false;
    }
    let options = NotificationOptions::new();
    options.set_body(body);
    // The tag is what makes a newer message replace the conversation's
    // banner instead of stacking one banner per message, and `renotify`
    // is what makes the replacement alert again: without it the browser
    // updates the banner silently and every message after the first is
    // missed by anyone away from the page.
    options.set_tag(tag);
    options.set_renotify(true);
    let icon_url = avatar().as_deref().and_then(|bytes| make_icon_url(bytes));
    if let Some(url) = icon_url.as_deref() {
        options.set_icon(url);
    }
    let notification = match Notification::new_with_options(title, &options) {
        Ok(notification) => notification,
        Err(error) => {
            // Mobile browsers can grant permission yet reject the page's
            // non-persistent Notification constructor. Pages already has
            // a service worker for isolation; it can show a persistent
            // banner and return its click through `message` instead.
            log::info!("page notification unavailable ({error:?}); trying service worker");
            if let Some(url) = icon_url.as_deref() {
                let _ = web_sys::Url::revoke_object_url(url);
            }
            return show_from_worker(tag, title, body);
        }
    };
    let id = next_live_id();
    let clicked_tag = tag.to_string();
    let clicked_notification = notification.clone();
    let onclick = Closure::new(move || {
        // The click is a user activation, so focusing is allowed here;
        // opening the conversation itself happens on the pump task. It is
        // also a moment a permission ask would be honoured, for the case
        // where an earlier prompt was dismissed without deciding.
        request_permission_for_prompt();
        if let Some(window) = web_sys::window() {
            let _ = window.focus();
        }
        push_activation(clicked_tag.clone());
        // Some browsers keep non-persistent banners open after activation.
        // `close` fires the close callback that removes the live entry;
        // keep that handler installed until the event has run.
        clicked_notification.set_onclick(None);
        clicked_notification.close();
    });
    notification.set_onclick(Some(onclick.as_ref().unchecked_ref()));
    // A dismissed banner cleans up after itself: without this every
    // conversation ever bannered keeps its notification, closures and
    // blob URL until reload. The id guard is what keeps the replacement
    // below honest — closing the old banner fires *its* close handler,
    // which must not take down the entry the new banner just installed.
    let closed_tag = tag.to_string();
    let onclose = Closure::new(move || {
        remove_live_notification(&closed_tag, id);
    });
    notification.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    LIVE.with(|live| {
        if let Ok(mut live) = live.try_borrow_mut() {
            if let Some(previous) = live.insert(
                tag.to_string(),
                LiveNotification {
                    id,
                    notification,
                    _onclick: onclick,
                    _onclose: onclose,
                    icon_url,
                },
            ) {
                // `close()` dispatches `close` later: remove both JS
                // references before dropping the Rust closures, or the
                // delayed event invokes a destroyed wasm-bindgen closure.
                previous.notification.set_onclose(None);
                previous.notification.set_onclick(None);
                previous.notification.close();
                if let Some(url) = previous.icon_url.as_deref() {
                    let _ = web_sys::Url::revoke_object_url(url);
                }
            }
        }
    });
    true
}

/// Drop one live banner's entry, revoking its icon URL — but only while
/// the entry is still the banner asking. A replacement installs the new
/// banner first and closes the old one after, so the old banner's own
/// close event lands on an entry that is no longer its to remove.
fn remove_live_notification(tag: &str, id: u64) {
    LIVE.with(|live| {
        if let Ok(mut live) = live.try_borrow_mut() {
            let stale = live.get(tag).is_some_and(|entry| entry.id == id);
            if stale {
                if let Some(entry) = live.remove(tag) {
                    // This callback may still be running; detach it
                    // before its owning closure leaves the live map.
                    entry.notification.set_onclose(None);
                    entry.notification.set_onclick(None);
                    if let Some(url) = entry.icon_url.as_deref() {
                        let _ = web_sys::Url::revoke_object_url(url);
                    }
                }
            }
        }
    });
}

/// Install the click bridge before a worker can show a banner. A worker
/// click has no access to this page's GPUI entity, so the worker focuses
/// a client and sends back the same opaque tag desktop GPUI reports.
fn watch_worker_clicks() {
    WORKER_CLICKS.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        let Some(window) = web_sys::window() else {
            return;
        };
        let worker = window.navigator().service_worker();
        let handler = Closure::new(move |event: web_sys::MessageEvent| {
            let epoch =
                js_sys::Reflect::get(&event.data(), &JsValue::from_str("oxidezapAccountEpoch"))
                    .ok()
                    .and_then(|value| value.as_string());
            if epoch.as_deref() != Some(&ACCOUNT_EPOCH.load(Ordering::Relaxed).to_string()) {
                // A notification shown for a departed account must not
                // select a JID in the account paired afterward.
                return;
            }
            let Ok(tag) =
                js_sys::Reflect::get(&event.data(), &JsValue::from_str("oxidezapNotificationTag"))
            else {
                return;
            };
            if let Some(tag) = tag
                .as_string()
                .filter(|tag| tag.starts_with("oxidezap-chat-"))
            {
                push_activation(tag);
            }
        });
        worker.set_onmessage(Some(handler.as_ref().unchecked_ref()));
        *slot = Some(handler);
        identify_to_worker(&worker);
    });
}

fn identify_to_worker(worker: &web_sys::ServiceWorkerContainer) {
    let Some(controller) = worker.controller() else {
        return;
    };
    let data = js_sys::Object::new();
    let id = crate::platform::front_end_id().to_string();
    if js_sys::Reflect::set(
        &data,
        &JsValue::from_str("oxidezapClientId"),
        &JsValue::from_str(&id),
    )
    .is_ok()
    {
        let _ = controller.post_message(&data);
    }
}

/// Mobile browsers reject `new Notification` even with permission granted.
/// `ready` resolves once the existing isolation worker controls the page;
/// this async fallback does not delay the UI's incoming-message path.
fn show_from_worker(tag: &str, title: &str, body: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    watch_worker_clicks();
    let worker = window.navigator().service_worker();
    let ready = match worker.ready() {
        Ok(ready) => ready,
        Err(error) => {
            log::warn!("notification worker unavailable: {error:?}");
            return false;
        }
    };
    let tag = tag.to_string();
    let title = title.to_string();
    let body = body.to_string();
    let epoch = ACCOUNT_EPOCH.load(Ordering::Relaxed);
    oxidezap_platform::spawn(async move {
        let Ok(registration) = wasm_bindgen_futures::JsFuture::from(ready).await else {
            log::warn!("notification worker did not become ready");
            return;
        };
        if ACCOUNT_EPOCH.load(Ordering::Relaxed) != epoch {
            return;
        }
        let registration: web_sys::ServiceWorkerRegistration = registration.unchecked_into();
        identify_to_worker(&worker);
        let options = NotificationOptions::new();
        options.set_body(&body);
        options.set_tag(&tag);
        options.set_renotify(true);
        let data = js_sys::Object::new();
        if js_sys::Reflect::set(
            &data,
            &JsValue::from_str("oxidezapTag"),
            &JsValue::from_str(&tag),
        )
        .is_err()
            || js_sys::Reflect::set(
                &data,
                &JsValue::from_str("oxidezapClientId"),
                &JsValue::from_str(&crate::platform::front_end_id().to_string()),
            )
            .is_err()
            || js_sys::Reflect::set(
                &data,
                &JsValue::from_str("oxidezapAccountEpoch"),
                &JsValue::from_str(&epoch.to_string()),
            )
            .is_err()
        {
            return;
        }
        options.set_data(&data);
        match registration.show_notification_with_options(&title, &options) {
            Ok(sent) => {
                if let Err(error) = wasm_bindgen_futures::JsFuture::from(sent).await {
                    log::warn!("browser refused worker notification: {error:?}");
                }
            }
            Err(error) => log::warn!("browser refused worker notification: {error:?}"),
        }
    });
    true
}

/// Stage the avatar as a blob URL the banner can show.
///
/// The bytes are copied into a JS-owned array first: the module is built
/// with shared memory, so a view over wasm memory is a shared
/// ArrayBufferView the Blob constructor refuses (see /clippy.toml).
fn make_icon_url(bytes: &[u8]) -> Option<String> {
    // A banner icon is a thumbnail; never retain megabytes of blob URL
    // for one. The macOS path caps attachments at 10 MiB for the same
    // reason at a different scale.
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        return None;
    };
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &bag).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

pub(super) fn clear_notifications() {
    // Invalidate worker posts still awaiting `ready` before they can
    // create a banner for the account that has just gone.
    let old_epoch = ACCOUNT_EPOCH.fetch_add(1, Ordering::Relaxed).to_string();
    if let Some(window) = web_sys::window() {
        let worker = window.navigator().service_worker();
        if let Ok(ready) = worker.ready() {
            oxidezap_platform::spawn(async move {
                let Ok(registration) = wasm_bindgen_futures::JsFuture::from(ready).await else {
                    return;
                };
                let registration: web_sys::ServiceWorkerRegistration =
                    registration.unchecked_into();
                let Ok(notifications) = registration.get_notifications() else {
                    return;
                };
                let Ok(notifications) = wasm_bindgen_futures::JsFuture::from(notifications).await
                else {
                    return;
                };
                let notifications = js_sys::Array::from(&notifications);
                let own_id = crate::platform::front_end_id().to_string();
                for item in notifications.iter() {
                    let notification: Notification = item.unchecked_into();
                    let id = js_sys::Reflect::get(
                        &notification.data(),
                        &JsValue::from_str("oxidezapClientId"),
                    )
                    .ok()
                    .and_then(|value| value.as_string());
                    let epoch = js_sys::Reflect::get(
                        &notification.data(),
                        &JsValue::from_str("oxidezapAccountEpoch"),
                    )
                    .ok()
                    .and_then(|value| value.as_string());
                    if id.as_deref() == Some(&own_id) && epoch.as_deref() == Some(&old_epoch) {
                        notification.close();
                    }
                }
            });
        }
    }
    // Clear queued clicks first; callbacks from closing old banners must
    // not be able to select a chat in the next account.
    if let Ok(mut queue) = activations().lock() {
        queue.tags.clear();
    }
    LIVE.with(|live| {
        let Ok(mut live) = live.try_borrow_mut() else {
            return;
        };
        // Remove before closing: a close event must not see an old entry.
        for (_, entry) in live.drain() {
            // A manual close queues a later close event. Detach its
            // handlers before dropping their Rust owners.
            entry.notification.set_onclose(None);
            entry.notification.set_onclick(None);
            entry.notification.close();
            if let Some(url) = entry.icon_url.as_deref() {
                let _ = web_sys::Url::revoke_object_url(url);
            }
        }
    });
}

fn push_activation(tag: String) {
    // Queued and the pump woken under the one lock; the wake itself
    // happens after it is released, so a woken poll never blocks on us.
    let waker = activations()
        .lock()
        .map(|mut queue| {
            queue.tags.push_back(tag);
            queue.waker.take()
        })
        .unwrap_or(None);
    if let Some(waker) = waker {
        waker.wake();
    }
}

pub(super) async fn next_notification_activation() -> String {
    poll_fn(|cx| {
        if let Ok(mut queue) = activations().lock() {
            if let Some(tag) = queue.tags.pop_front() {
                return Poll::Ready(tag);
            }
            // Stored before returning, under the same lock that guards
            // the queue: a click landing between the pop and the store
            // either precedes the store, and the pop above already saw
            // it, or follows it, and the push wakes what was stored.
            queue.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    })
    .await
}
