//! Handing a file to the person using the window.
//!
//! On the desktop that is a path: the Downloads directory, a name the sender
//! cannot use to escape it, and a `(n)` suffix so a save never clobbers
//! anything. A page has none of that — no environment to read a directory
//! out of and no filesystem to write into — but it has the same *gesture*
//! available, because handing a file to the user is something a browser does
//! natively: a blob, an object URL, and a link that carries `download`.
//!
//! So both answer the same question — here are some bytes and a name, put
//! them where the user keeps things — and return a description of where they
//! went, because on one of the two there is no path to report.

/// Save these bytes under this name.
///
/// Returns where they landed, in words, for the line that says so.
///
/// # Errors
///
/// Nowhere to write, or writing failed.
pub fn save(file_name: &str, data: &[u8]) -> Result<String, String> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::save(file_name, data)
    }
    #[cfg(target_family = "wasm")]
    {
        web::save(file_name, data)
    }
}

/// Whether saving happens somewhere other than the calling thread.
///
/// The desktop write is blocking I/O and belongs on the background executor.
/// The web one reaches for `document`, which exists on one thread only — and
/// gpui's background executor is a real worker there, so moving it would put
/// it somewhere `document` is `None`. The call sites ask this rather than
/// carrying a `cfg` of their own.
pub const SAVES_OFF_THREAD: bool = cfg!(not(target_family = "wasm"));

#[cfg(target_family = "wasm")]
mod web {
    use wasm_bindgen::JsCast as _;

    /// Hand the bytes to the browser's own download machinery.
    ///
    /// A blob, an object URL, and an `<a download>` clicked from script. The
    /// anchor never joins the document — it does not need to, and a page that
    /// visibly grew a link for a moment would be a page with a flicker in it.
    /// The URL is revoked straight away: the click has already taken its own
    /// reference to the blob.
    pub fn save(file_name: &str, data: &[u8]) -> Result<String, String> {
        let window = web_sys::window().ok_or("no window to save from")?;
        let document = window.document().ok_or("no document to save from")?;

        // Copied into a JS array first: `Blob` takes a JS value, and handing
        // it a view over wasm memory would let a later allocation move the
        // bytes out from under it.
        let bytes = js_sys::Uint8Array::from(data);
        let parts = js_sys::Array::new();
        parts.push(&bytes.buffer());
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
            .map_err(|e| format!("the browser refused the file: {e:?}"))?;
        let url = web_sys::Url::create_object_url_with_blob(&blob)
            .map_err(|e| format!("the browser refused a link to the file: {e:?}"))?;

        let anchor = document
            .create_element("a")
            .map_err(|e| format!("the browser refused a link: {e:?}"))?
            .dyn_into::<web_sys::HtmlAnchorElement>()
            .map_err(|_| "the browser made something that is not a link".to_string())?;
        anchor.set_href(&url);
        anchor.set_download(file_name);
        anchor.click();
        let _ = web_sys::Url::revoke_object_url(&url);

        Ok(format!("{file_name} (your browser's downloads)"))
    }
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::io::Write as _;
    use std::path::PathBuf;

    /// Write into the user's Downloads directory.
    ///
    /// `$XDG_DOWNLOAD_DIR`, then `$HOME` or `%USERPROFILE%` + `/Downloads`,
    /// then the working directory — the same fallback chain the database
    /// uses when no home is known.
    pub fn save(file_name: &str, data: &[u8]) -> Result<String, String> {
        write(file_name, data)
            .map(|path| path.display().to_string())
            .map_err(|e| e.to_string())
    }

    fn write(file_name: &str, data: &[u8]) -> std::io::Result<PathBuf> {
        let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(PathBuf::from(v));
        let dir = std::env::var_os("XDG_DOWNLOAD_DIR")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("HOME")
                    .and_then(not_empty)
                    .or_else(|| std::env::var_os("USERPROFILE").and_then(not_empty))
                    .map(|home| home.join("Downloads"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&dir)?;

        // The name comes off the wire: strip path separators (and `:`, which
        // on Windows makes a drive-relative path) so a hostile sender can't
        // traverse out of the directory.
        let sanitized: String = file_name
            .chars()
            .map(|c| {
                if std::path::is_separator(c) || c == '\\' || c == ':' || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let name = match sanitized.trim() {
            "" | "." | ".." => "document",
            trimmed => trimmed,
        };

        // Windows treats device basenames (CON, NUL, COM1…) as reserved for
        // any extension; prefix them so the save can't resolve to a device.
        let stem = name
            .split_once('.')
            .map_or(name, |(stem, _)| stem)
            .trim_end_matches([' ', '.'])
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        let name = if reserved {
            format!("_{name}")
        } else {
            name.to_string()
        };

        // create_new + " (n)" suffixing so a download never clobbers an
        // existing file of the same name.
        for attempt in 0..1000u32 {
            let candidate = if attempt == 0 {
                dir.join(&name)
            } else {
                let (stem, extension) = name
                    .rsplit_once('.')
                    .map_or((name.as_str(), ""), |(stem, ext)| (stem, ext));
                let suffixed = if extension.is_empty() {
                    format!("{stem} ({attempt})")
                } else {
                    format!("{stem} ({attempt}).{extension}")
                };
                dir.join(suffixed)
            };
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    file.write_all(data)?;
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("a thousand files are already called {name}"),
        ))
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    /// The name comes off the wire. A sender who names a file `../../.ssh/id`
    /// must not reach outside the Downloads directory.
    #[test]
    fn a_hostile_name_cannot_leave_the_directory() {
        let dir = std::env::temp_dir().join(format!("oxidezap-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        // SAFETY: single-threaded test process for this variable's lifetime.
        unsafe { std::env::set_var("XDG_DOWNLOAD_DIR", &dir) };

        let where_it_went = save("../escaped.txt", b"nope").expect("a save");
        assert!(
            where_it_went.starts_with(&dir.display().to_string()),
            "{where_it_went} left {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
