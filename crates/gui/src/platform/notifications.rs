//! Early system-notification authorization.
//!
//! GPUI requests authorization lazily from `show_system_notification` and
//! immediately submits that same first notification. Asking after GPUI has
//! installed its response delegate, but before a message can arrive, avoids
//! making the first incoming message race the macOS permission sheet.

/// Ask the operating system for notification authorization when it has one.
pub fn request_authorization() {
    imp::request_authorization();
}

/// Post a native notification, attaching an already-cached profile image when
/// the bytes are a format macOS can thumbnail.
///
/// This is intentionally a best-effort presentation API. The supplied reader
/// only accesses the existing cache, off the UI thread, and never waits for an
/// avatar download. When
/// `avatar` is absent or unsupported, the same notification is posted without
/// an attachment. The attachment is a media preview, not a sender avatar: the
/// macOS sender-avatar treatment belongs to Communication Notifications and
/// requires an `INSendMessageIntent` plus the app capability/entitlements.
///
/// Returns `true` when the native path accepted the request. A non-macOS build,
/// or a process not launched from an app bundle, returns `false` so callers
/// can retain their normal GPUI path. On the web the request is posted through
/// the browser's Notification API instead — GPUI's web backend leaves system
/// notifications a no-op — and `false` means permission is missing or the API
/// is unavailable, so the same GPUI fallback applies there too.
pub fn show_notification_with_avatar(
    tag: &str,
    title: &str,
    body: &str,
    avatar: impl Fn() -> Option<std::sync::Arc<Vec<u8>>> + Send + Sync + 'static,
) -> bool {
    imp::show_notification_with_avatar(tag, title, body, avatar)
}

/// Wait for the next web-notification click.
///
/// The browser hands a click to a JS callback, not to GPUI's response path,
/// so the callback queues the tag and the application's pump task awaits it
/// here, then opens the conversation through the ordinary
/// [`crate::app::WhatsAppApp::open_system_notification`] path. Away from the
/// web there is no such queue — clicks arrive through GPUI already — so this
/// never resolves and the pump task parks forever.
pub async fn next_notification_activation() -> String {
    imp::next_notification_activation().await
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::{HashMap, hash_map::DefaultHasher};
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex, OnceLock};

    use block2::RcBlock;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSArray, NSBundle, NSError, NSString, NSURL};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
        UNNotificationAttachment, UNNotificationRequest, UNNotificationSettings,
        UNNotificationSound, UNUserNotificationCenter,
    };
    use portable_atomic::{AtomicU64, Ordering};

    /// One submission lane per stable notification tag. Avatar reads happen
    /// on workers, so an older read can otherwise finish after a newer one
    /// and replace the newer banner. The generation check also covers a
    /// failed attachment's plain retry, which is delivered asynchronously.
    struct TagState {
        generation: u64,
        lane: Arc<Mutex<()>>,
    }

    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    static TAG_STATES: OnceLock<Mutex<HashMap<String, TagState>>> = OnceLock::new();

    fn begin_tag_submission(tag: &str) -> (u64, Arc<Mutex<()>>) {
        let states = TAG_STATES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut states = states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states.entry(tag.to_string()).or_insert_with(|| TagState {
            generation: 0,
            lane: Arc::new(Mutex::new(())),
        });
        state.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        (state.generation, Arc::clone(&state.lane))
    }

    fn is_current_tag_submission(tag: &str, generation: u64) -> bool {
        TAG_STATES
            .get()
            .and_then(|states| states.lock().ok())
            .and_then(|states| states.get(tag).map(|state| state.generation == generation))
            .unwrap_or(false)
    }

    fn may_retry_plain(tag: &str, generation: u64, has_attachment: bool) -> bool {
        has_attachment && is_current_tag_submission(tag, generation)
    }

    /// Unreachable by construction: clicks arrive through GPUI's response
    /// path here, so the pump task parked on this never wakes.
    pub(super) async fn next_notification_activation() -> String {
        std::future::pending().await
    }

    pub(super) fn request_authorization() {
        // The API raises an Objective-C exception outside an application
        // bundle. Keep `cargo run` and unit tests on the supported no-op path.
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            log::info!("system notification authorization skipped: not running from an app bundle");
            return;
        }

        let completion = RcBlock::new(|granted: Bool, error: *mut NSError| {
            // SAFETY: UserNotifications lends the NSError for this callback.
            if let Some(error) = unsafe { error.as_ref() } {
                log::warn!(
                    "system notification authorization failed: {}",
                    error.localizedDescription()
                );
            } else if !granted.as_bool() {
                log::info!("system notification authorization denied");
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &completion,
            );
    }

    pub(super) fn show_notification_with_avatar(
        tag: &str,
        title: &str,
        body: &str,
        avatar: impl Fn() -> Option<Arc<Vec<u8>>> + Send + Sync + 'static,
    ) -> bool {
        // UserNotifications raises an Objective-C exception outside an app
        // bundle. Keep direct launches and tests on the GPUI/no-op path.
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            log::info!("system notification skipped: not running from an app bundle");
            return false;
        }

        // Check *before* scheduling: requests queued while the user is still
        // deciding the permission sheet can appear as stale alerts much later.
        // The callback also keeps the UI free of disk reads and attachment
        // encoding. A denied/undetermined message is not queued for later.
        let tag = tag.to_string();
        let title = title.to_string();
        let body = body.to_string();
        let avatar = Arc::new(avatar);
        let (generation, lane) = begin_tag_submission(&tag);
        let settings = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: UserNotifications lends a live settings object for this callback.
            let status = unsafe { settings.as_ref() }.authorizationStatus();
            if !matches!(
                status,
                UNAuthorizationStatus::Authorized
                    | UNAuthorizationStatus::Provisional
                    | UNAuthorizationStatus::Ephemeral
            ) {
                return;
            }
            let (tag, title, body, avatar) = (
                tag.clone(),
                title.clone(),
                body.clone(),
                Arc::clone(&avatar),
            );
            let lane = Arc::clone(&lane);
            std::thread::spawn(move || {
                let bytes = avatar();
                autoreleasepool(|_| {
                    post_authorized(
                        &tag,
                        &title,
                        &body,
                        bytes.as_deref().map(Vec::as_slice),
                        generation,
                        lane,
                    );
                });
            });
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .getNotificationSettingsWithCompletionHandler(&settings);
        true
    }

    fn post_authorized(
        tag: &str,
        title: &str,
        body: &str,
        avatar: Option<&[u8]>,
        generation: u64,
        lane: Arc<Mutex<()>>,
    ) {
        let _submission = lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));

        let attachment = avatar.and_then(|bytes| make_avatar_attachment(tag, bytes));
        if !is_current_tag_submission(tag, generation) {
            if let Some((_, path)) = attachment {
                let _ = std::fs::remove_file(path);
            }
            return;
        }
        if let Some((image, _)) = &attachment {
            content.setAttachments(&NSArray::from_retained_slice(std::slice::from_ref(image)));
        }
        content.setSound(Some(&UNNotificationSound::defaultSound()));

        // A nil trigger delivers immediately. The stable tag has the same
        // replacement semantics as GPUI's notification backend.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(tag),
            &content,
            None,
        );
        let retry_without_avatar = attachment.is_some();
        let cleanup_path = attachment.map(|(_, path)| path);
        let retry_tag = tag.to_string();
        let retry_title = title.to_string();
        let retry_body = body.to_string();
        let retry_lane = Arc::clone(&lane);
        let completion = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: when non-null, UserNotifications lends an NSError for
            // the duration of this callback.
            if let Some(error) = unsafe { error.as_ref() } {
                log::warn!(
                    "failed to deliver system notification: {}",
                    error.localizedDescription()
                );
                if let Some(path) = &cleanup_path {
                    let _ = std::fs::remove_file(path);
                }
                // A corrupted/oversized attachment must not eat the message
                // alert. Re-submit once without media; the same request id
                // replaces any pending first attempt, without a second loop.
                if retry_without_avatar {
                    // Queue the retry off the callback. Although Apple's
                    // implementation calls this asynchronously, keeping the
                    // retry off-stack also makes the per-tag lane safe if a
                    // test double or a future implementation invokes the
                    // completion inline while `post_authorized` still owns
                    // the lane lock.
                    let retry_lane = Arc::clone(&retry_lane);
                    let retry_tag = retry_tag.clone();
                    let retry_title = retry_title.clone();
                    let retry_body = retry_body.clone();
                    std::thread::spawn(move || {
                        let _submission = retry_lane
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if may_retry_plain(&retry_tag, generation, retry_without_avatar) {
                            submit_plain(&retry_tag, &retry_title, &retry_body);
                        }
                    });
                }
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    }

    fn submit_plain(tag: &str, title: &str, body: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(tag),
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    }

    /// Stage a private copy because UNNotificationAttachment accepts a file
    /// URL, while the media cache deliberately exposes bytes. On successful
    /// scheduling Notification Center moves the file into its own store.
    fn make_avatar_attachment(
        tag: &str,
        bytes: &[u8],
    ) -> Option<(objc2::rc::Retained<UNNotificationAttachment>, PathBuf)> {
        // Apple limits image attachments to 10 MiB. Do not let an oversized
        // cached blob cause the whole text notification to be rejected.
        if bytes.len() > 10 * 1024 * 1024 {
            return None;
        }
        let extension = image_extension(bytes)?;
        let path = attachment_path(tag, bytes, extension);
        write_attachment_file(&path, bytes)?;
        let url = NSURL::from_file_path(&path)?;
        let identifier = NSString::from_str(path.file_stem()?.to_str()?);

        // SAFETY: `identifier` and `url` are valid Objective-C objects for the
        // duration of the call, and a nil options dictionary is supported by
        // the API. The result validates the file's image type.
        match unsafe {
            UNNotificationAttachment::attachmentWithIdentifier_URL_options_error(
                &identifier,
                &url,
                None,
            )
        } {
            Ok(attachment) => Some((attachment, path)),
            Err(error) => {
                log::debug!(
                    "profile image was not accepted as a notification attachment: {}",
                    error.localizedDescription()
                );
                let _ = std::fs::remove_file(path);
                None
            }
        }
    }

    fn attachment_path(tag: &str, bytes: &[u8], extension: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        tag.hash(&mut hasher);
        bytes.hash(&mut hasher);

        let mut directory = std::env::temp_dir();
        directory.push("oxidezap-notification-avatars");
        directory.push(format!("{:016x}.{extension}", hasher.finish()));
        directory
    }

    fn write_attachment_file(path: &Path, bytes: &[u8]) -> Option<()> {
        let directory = path.parent()?;
        std::fs::create_dir_all(directory).ok()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).ok()?;
        }

        std::fs::write(path, bytes).ok()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok()?;
        }
        Some(())
    }

    fn image_extension(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("png")
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some("jpg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("gif")
        } else {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_new_generation_invalidates_only_the_same_tag() {
            let (old, old_lane) = begin_tag_submission("test-notification-stale");
            let (current, current_lane) = begin_tag_submission("test-notification-stale");

            assert_ne!(old, current);
            assert!(!is_current_tag_submission("test-notification-stale", old));
            assert!(is_current_tag_submission(
                "test-notification-stale",
                current
            ));
            assert!(Arc::ptr_eq(&old_lane, &current_lane));
            assert!(may_retry_plain("test-notification-stale", current, true));
            assert!(!may_retry_plain("test-notification-stale", old, true));
            assert!(!may_retry_plain("test-notification-stale", current, false));
        }

        #[test]
        fn independent_tags_keep_independent_generations_and_lanes() {
            let (first, first_lane) = begin_tag_submission("test-notification-first");
            let (second, second_lane) = begin_tag_submission("test-notification-second");

            assert!(is_current_tag_submission("test-notification-first", first));
            assert!(is_current_tag_submission(
                "test-notification-second",
                second
            ));
            assert!(!Arc::ptr_eq(&first_lane, &second_lane));

            let (next_first, next_first_lane) = begin_tag_submission("test-notification-first");
            assert!(!is_current_tag_submission("test-notification-first", first));
            assert!(is_current_tag_submission(
                "test-notification-first",
                next_first
            ));
            assert!(is_current_tag_submission(
                "test-notification-second",
                second
            ));
            assert!(Arc::ptr_eq(&first_lane, &next_first_lane));
        }
    }
}

/// The page: GPUI's web backend leaves system notifications a no-op, so the
/// browser's Notification API stands in. One live notification per stable tag
/// mirrors the replacement semantics the desktop path gets from GPUI — a
/// newer message in the same conversation replaces its banner — and the
/// click handler focuses the window and queues the tag for the application's
/// pump task (see [`super::next_notification_activation`]).
#[cfg(target_family = "wasm")]
mod imp {
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
    }

    use futures_lite::future::poll_fn;
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

    pub(super) fn request_authorization() {
        // Best effort: where the browser wants transient activation for the
        // prompt, this startup ask is ignored and the post-time ask below is
        // the one that counts — a message arriving while the user is in the
        // page — so an early grant here only ever saves that later round.
        request_permission_for_prompt();
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
            return false;
        }
        if Notification::permission() == NotificationPermission::Default {
            // A message arriving mid-interaction carries its own gesture, so
            // this ask can prompt where the startup one could not. The banner
            // itself still waits for the grant: this post stays silent and
            // the next message finds the permission settled.
            request_permission_for_prompt();
            return false;
        }
        if Notification::permission() != NotificationPermission::Granted {
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
                log::debug!("browser refused the notification: {error:?}");
                if let Some(url) = icon_url.as_deref() {
                    let _ = web_sys::Url::revoke_object_url(url);
                }
                return false;
            }
        };
        let id = next_live_id();
        let clicked_tag = tag.to_string();
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
                        if let Some(url) = entry.icon_url.as_deref() {
                            let _ = web_sys::Url::revoke_object_url(url);
                        }
                    }
                }
            }
        });
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
}

#[cfg(not(any(target_os = "macos", target_family = "wasm")))]
mod imp {
    pub(super) const fn request_authorization() {}

    pub(super) fn show_notification_with_avatar(
        _tag: &str,
        _title: &str,
        _body: &str,
        _avatar: impl Fn() -> Option<std::sync::Arc<Vec<u8>>> + Send + Sync + 'static,
    ) -> bool {
        false
    }

    /// Unreachable by construction: clicks arrive through GPUI's response
    /// path on every platform that compiles this half, so the pump task
    /// parked on this never wakes.
    pub(super) async fn next_notification_activation() -> String {
        std::future::pending().await
    }
}
