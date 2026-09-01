//! Choosing files to send.
//!
//! One question — which files, and what is in them — and two ways of asking
//! it. A desktop front end asks the operating system through gpui and reads
//! the paths it gets back; a page asks the browser for a `<input type=file>`
//! and reads the `File` objects, because a page has no paths and no
//! filesystem to resolve one against.
//!
//! Both hand back the same thing, and it is deliberately *bytes* rather than a
//! handle: what happens next is a staged upload, which needs the whole file in
//! memory on either platform — the daemon reads a payload whole because a
//! half-staged file under a key a send is about to name is worse than a
//! refused one.
//!
//! # Why the ceiling is checked here
//!
//! A file's size is knowable before its bytes are: `metadata` on one side and
//! `File.size` on the other. Reading a file that cannot be staged costs the
//! whole read — and, in a page, a copy of it in a linear memory that has a
//! ceiling — to arrive at a refusal the length alone already decided. So the
//! ceiling is the protocol's ([`oxidezap_ipc::MAX_STAGED_BYTES`]) and it is
//! applied before anything is read.

/// A file somebody chose, read.
pub struct Picked {
    /// What it was called where it was picked. Sanitized by whoever writes
    /// it, here and on the other side alike — this is a name, not a path.
    pub file_name: String,
    /// What it is, as far as the platform will say.
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// What one trip to the file chooser produced.
///
/// Two lists rather than a `Result` per file: picking four photos and one
/// film should send the four and say what happened to the fifth, which a
/// single failure cannot express and a silently shortened list does not say.
#[derive(Default)]
pub struct Chosen {
    /// The files that can be sent, in the order they were picked.
    pub files: Vec<Picked>,
    /// One sentence per file that cannot be, written for a reader.
    pub refused: Vec<String>,
}

impl Chosen {
    /// Whether the chooser was dismissed without choosing anything, which is
    /// not a failure and is not worth a line on screen.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.refused.is_empty()
    }
}

/// Ask for files to send.
///
/// Called from the window's own thread, because that is where both halves
/// have to start: gpui's prompt belongs to the platform window, and a
/// browser's file input belongs to the document. What the future does *after*
/// the choice is where the two differ, and neither call site has to know:
/// the desktop reads are blocking I/O and go to the background executor from
/// inside the future, while a page's are promises on the one thread it has.
///
/// # Errors
///
/// The chooser could not be opened, or it went away without answering.
pub fn choose(cx: &gpui::App) -> impl Future<Output = Result<Chosen, String>> + use<> {
    imp::choose(cx)
}

/// What a file of this name most likely is.
///
/// A table rather than a crate: this is used for the handful of types a
/// person actually attaches, the answer only has to be good enough for the
/// recipient's client to pick a viewer, and `application/octet-stream` is a
/// perfectly honest answer for anything else — a document promises nothing
/// about what is inside it.
///
/// Native has nothing else to go on; a page uses it only where the browser
/// declined to say (`File.type` is empty for types it does not recognise).
#[must_use]
pub fn mime_for_name(file_name: &str) -> &'static str {
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",

        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "3gp" => "video/3gpp",

        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",

        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "gz" => "application/gzip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "epub" => "application/epub+zip",
        "apk" => "application/vnd.android.package-archive",

        _ => "application/octet-stream",
    }
}

/// What one trip to the chooser may hold in memory at once.
///
/// The per-file ceiling bounds one payload; without this, nothing bounds the
/// *selection*. A chooser hands back everything at once, so ten files just
/// under the ceiling are two thirds of a gigabyte read and held before the
/// first of them is staged — which on a page is the linear memory the whole
/// interface is drawn out of, and on a desktop is still a process the person
/// is using.
///
/// The same number rather than a second one, and that is the argument for it:
/// a trip to the chooser may cost what one file may cost. Ten photos are a
/// normal send and fit; ten films are not one act, and the files past the
/// budget are refused by name rather than silently dropped.
const SELECTION_BUDGET_BYTES: u64 = oxidezap_ipc::MAX_STAGED_BYTES;

/// Tracks what a selection has read so far, and refuses what would not fit.
///
/// Held by each half of [`choose`] across its own loop, so the answer is one
/// rule rather than two: the desktop reads files one at a time from paths and
/// a page reads them one at a time from a `FileList`, and both ask this
/// before they read rather than after.
#[derive(Default)]
struct Budget {
    taken: u64,
}

impl Budget {
    /// Why this file cannot join what is already read, or `None` if it can —
    /// counting it when it can.
    fn refuse(&mut self, file_name: &str, size: u64) -> Option<String> {
        if let Some(refusal) = unsendable(file_name, size) {
            return Some(refusal);
        }
        match self.taken.checked_add(size) {
            Some(total) if total <= SELECTION_BUDGET_BYTES => {
                self.taken = total;
                None
            }
            _ => Some(format!(
                "{file_name} did not fit: one trip to the file chooser can \
                 carry {} in total. Send it on its own.",
                in_mib(SELECTION_BUDGET_BYTES)
            )),
        }
    }
}

/// Why a file of this size cannot be sent, or `None` if it can.
///
/// A sentence, because it is drawn as one, and it names the file and both
/// numbers: "too big" alone leaves somebody guessing which of the four files
/// they picked was the problem.
///
/// Asked of the length rather than of the bytes, which is the whole point —
/// see the note at the top of this file. An empty file is refused here for a
/// different reason and in the same place: it is the one length that produces
/// a message the recipient cannot open, and finding that out at the CDN would
/// mean uploading nothing to be told nothing.
#[must_use]
pub fn unsendable(file_name: &str, size: u64) -> Option<String> {
    let ceiling = oxidezap_ipc::MAX_STAGED_BYTES;
    if size == 0 {
        return Some(format!("{file_name} is empty."));
    }
    (size > ceiling).then(|| {
        format!(
            "{file_name} is {} and the most that can be sent is {}.",
            in_mib(size),
            in_mib(ceiling)
        )
    })
}

/// A byte count as a person reads one.
///
/// MiB rather than MB, because the number above it is a power of two and the
/// two are not the same: 64 MiB printed as "64.0 MB" understates the ceiling
/// by three megabytes, which matters exactly when somebody is looking at this
/// sentence to decide whether a file will fit.
fn in_mib(bytes: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a figure printed to one decimal place; the exact byte count is not the point"
    )]
    let mib = bytes as f64 / (1024.0 * 1024.0);
    format!("{mib:.1} MiB")
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::path::{Path, PathBuf};

    use super::{Chosen, Picked};

    /// The platform's own file chooser, then a read off the UI thread.
    ///
    /// The prompt has to be asked for here — it belongs to the platform
    /// window — and the reads must not be: opening four photos is four
    /// synchronous reads of several megabytes each, and the window draws
    /// nothing while they run.
    pub(super) fn choose(cx: &gpui::App) -> impl Future<Output = Result<Chosen, String>> + use<> {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        let executor = cx.background_executor().clone();
        async move {
            let paths = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                // Dismissed, which is an answer rather than a failure.
                Ok(Ok(None)) => return Ok(Chosen::default()),
                Ok(Err(e)) => return Err(format!("the file chooser could not be opened: {e}")),
                Err(_) => return Err("the file chooser closed without answering".to_string()),
            };
            Ok(executor.spawn(async move { read_all(&paths) }).await)
        }
    }

    /// Read what was picked, keeping what can be sent and saying what cannot.
    ///
    /// One budget across the whole selection, asked before each read: four
    /// photos are read and held together, and nothing else bounds that.
    fn read_all(paths: &[PathBuf]) -> Chosen {
        let mut chosen = Chosen::default();
        let mut budget = super::Budget::default();
        for path in paths {
            match read_one(path, &mut budget) {
                Ok(picked) => chosen.files.push(picked),
                Err(refusal) => chosen.refused.push(refusal),
            }
        }
        chosen
    }

    /// One file, or the sentence to show instead.
    fn read_one(path: &Path, budget: &mut super::Budget) -> Result<Picked, String> {
        // The last component, and never the path: this becomes the name on
        // the message, and where the file was is nobody else's business.
        let file_name = path.file_name().map_or_else(
            || "file".to_string(),
            |name| name.to_string_lossy().into_owned(),
        );

        // Asked before the read, so an oversized file costs a `stat` rather
        // than its own size in memory — and so does one the selection has no
        // room left for.
        let size = std::fs::metadata(path)
            .map_err(|e| format!("{file_name} could not be opened: {e}"))?
            .len();
        if let Some(refusal) = budget.refuse(&file_name, size) {
            return Err(refusal);
        }

        let bytes =
            std::fs::read(path).map_err(|e| format!("{file_name} could not be read: {e}"))?;
        Ok(Picked {
            mime_type: super::mime_for_name(&file_name).to_string(),
            file_name,
            bytes,
        })
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;
    use wasm_bindgen_futures::JsFuture;

    use super::{Chosen, Picked};

    /// A file input, clicked from script, then one read per file.
    ///
    /// The same element the plugin installer uses and for the same reasons:
    /// `showOpenFilePicker` is Chromium-only and wants a secure context the
    /// published page has but a developer's `trunk serve` may not. The input
    /// joins the document for the length of the gesture and is taken out
    /// again — a detached input's `click()` is ignored outright by some
    /// engines, and one that stays is a control the page grew and never lost.
    pub(super) fn choose(_cx: &gpui::App) -> impl Future<Output = Result<Chosen, String>> + use<> {
        async move {
            let files = match files_picked().await {
                Some(files) => files,
                // Dismissed, or there is no document to ask in. Neither is
                // worth a line on screen.
                None => return Ok(Chosen::default()),
            };

            let mut chosen = Chosen::default();
            // One budget across the whole selection; see `Budget`.
            let mut budget = super::Budget::default();
            for index in 0..files.length() {
                let Some(file) = files.get(index) else {
                    continue;
                };
                let file_name = file.name();
                // Before the read: a `File` knows its length without anybody
                // asking for its bytes, and reading one that cannot be sent
                // costs the tab twice its size — the `ArrayBuffer` and the
                // copy out of it — to reach a refusal the length decided.
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "`File.size` is a byte count: a non-negative integer in an f64"
                )]
                let size = file.size() as u64;
                if let Some(refusal) = budget.refuse(&file_name, size) {
                    chosen.refused.push(refusal);
                    continue;
                }

                match JsFuture::from(file.array_buffer()).await {
                    Ok(buffer) => chosen.files.push(Picked {
                        // The browser's own answer, and this table only where
                        // it declined to give one: `File.type` is empty for
                        // every type the agent does not recognise.
                        mime_type: match file.type_() {
                            declared if declared.is_empty() => {
                                super::mime_for_name(&file_name).to_string()
                            }
                            declared => declared,
                        },
                        bytes: Uint8Array::new(&buffer).to_vec(),
                        file_name,
                    }),
                    Err(e) => chosen
                        .refused
                        .push(format!("{file_name} could not be read ({e:?}).")),
                }
            }
            Ok(chosen)
        }
    }

    /// What somebody picked, or `None` if they picked nothing.
    ///
    /// Two events, because a browser has two ways of ending this: `change`
    /// when files were chosen, and `cancel` when the dialog was dismissed.
    /// Waiting only for the first leaves the task — and the closures it holds
    /// — alive for the life of the page every time somebody changes their
    /// mind.
    async fn files_picked() -> Option<web_sys::FileList> {
        let document = web_sys::window()?.document()?;
        let input: web_sys::HtmlInputElement =
            document.create_element("input").ok()?.dyn_into().ok()?;
        input.set_type("file");
        input.set_multiple(true);
        let style = input.style();
        let _ = style.set_property("display", "none");
        let body = document.body()?;
        body.append_child(&input).ok()?;

        let (tx, rx) = futures_channel::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let done = Closure::<dyn FnMut()>::new(move || {
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        });
        let handler = done.as_ref().unchecked_ref();
        let _ = input.add_event_listener_with_callback("change", handler);
        let _ = input.add_event_listener_with_callback("cancel", handler);
        input.click();
        let _ = rx.await;
        let _ = body.remove_child(&input);

        input.files()
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, SELECTION_BUDGET_BYTES, mime_for_name, unsendable};

    /// The type decides what the message is sent *as*, so an extension nobody
    /// recognises has to land on the kind that promises nothing about its
    /// contents rather than on a wrong guess.
    #[test]
    fn a_name_says_what_a_file_is_or_admits_it_cannot() {
        assert_eq!(mime_for_name("praia.JPG"), "image/jpeg");
        assert_eq!(mime_for_name("clipe.mp4"), "video/mp4");
        assert_eq!(mime_for_name("nota.pdf"), "application/pdf");
        // No extension, an unknown one, and a name that is nothing but a dot
        // are all the same answer.
        for opaque in ["LEIAME", "arquivo.qwerty", ".", "arquivo."] {
            assert_eq!(
                mime_for_name(opaque),
                "application/octet-stream",
                "{opaque}"
            );
        }
        // A dot in the directory-ish part of a name must not be read as an
        // extension; only what follows the last one is.
        assert_eq!(mime_for_name("v1.2.tar.gz"), "application/gzip");
    }

    /// The ceiling is the protocol's, and the refusal names both numbers:
    /// "too big" alone leaves somebody guessing which of four files it was.
    /// Nothing has been read at this point, which is the point.
    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let ceiling = oxidezap_ipc::MAX_STAGED_BYTES;
        assert!(unsendable("clipe.mp4", ceiling).is_none());
        let refusal = unsendable("clipe.mp4", ceiling + 1).expect("past the ceiling");
        assert!(refusal.contains("clipe.mp4"), "{refusal}");
        assert!(refusal.contains("64.0 MiB"), "{refusal}");
    }

    /// A chooser hands back everything at once, so the *selection* needs a
    /// bound of its own: without one, ten files just under the per-file
    /// ceiling are read and held together. What is past the budget is refused
    /// by name, not silently dropped.
    #[test]
    fn a_selection_is_bounded_as_well_as_each_file_in_it() {
        let quarter = SELECTION_BUDGET_BYTES / 4;
        let mut budget = Budget::default();
        assert!(budget.refuse("um.jpg", quarter).is_none());
        assert!(budget.refuse("dois.jpg", quarter).is_none());
        // Half the budget is left, so the film does not fit — and it says so
        // by name rather than being dropped.
        let refusal = budget
            .refuse("filme.mp4", quarter * 3)
            .expect("past the budget");
        assert!(refusal.contains("filme.mp4"), "{refusal}");
        // And the photo after it still does: a refusal is about the file that
        // did not fit, not the end of the selection.
        assert!(budget.refuse("tres.jpg", quarter).is_none());
        // Exactly full is not over.
        assert!(budget.refuse("quatro.jpg", quarter).is_none());
        assert!(budget.refuse("cinco.jpg", 1).is_some());
    }

    /// The other length that cannot become a message: a recipient handed an
    /// empty attachment has something they cannot open, and the upload would
    /// have said nothing was wrong.
    #[test]
    fn an_empty_file_is_refused_too() {
        assert!(unsendable("vazio.txt", 0).is_some());
        assert!(unsendable("vazio.txt", 1).is_none());
    }
}
