//! Automatic image downloads require the session adapter's streaming budget.
//! Embedded browser sessions currently expose only buffered fetch. Their
//! plaintext metadata and manual downloads still work.

pub(crate) const AUTOMATIC: bool = imp::AUTOMATIC;

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub(super) const AUTOMATIC: bool = true;
}
#[cfg(target_family = "wasm")]
mod imp {
    pub(super) const AUTOMATIC: bool = false;
}
