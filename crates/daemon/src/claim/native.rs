//! Nothing to take here, and nothing that would take it.
//!
//! The process makes its claim in `main`, before anything else runs, and it
//! is a file lock over the state directory rather than something an embedded
//! service could ask for again — asking twice from one process would find
//! itself. What uses this module on a desktop is the test that wants the real
//! protocol without a socket, and one of those is one process.

/// Held for as long as the session is.
pub(crate) struct Claim;

impl Drop for Claim {
    /// Nothing to release, and the empty body is the point.
    ///
    /// The caller's contract is "hold this until the session ends", and it
    /// holds on both platforms — a value that is only *dropped* on one of
    /// them would be a value the caller has to think about. What a desktop's
    /// real claim is, `main` took before any of this ran, and it lasts as
    /// long as the process does.
    fn drop(&mut self) {}
}

/// Always granted: see the module note.
///
/// # Errors
///
/// Never, here. The browser's half can genuinely be refused.
pub(crate) async fn take() -> Result<Claim, String> {
    Ok(Claim)
}
