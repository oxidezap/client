use thiserror::Error;
use wacore::store::error::StoreError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChatStoreError {
    #[error("storage error")]
    Store(#[from] StoreError),

    #[error("invalid full-text search query")]
    InvalidSearchQuery,

    /// A writer batch rolled back; the writes acknowledged by this `flush`
    /// were dropped. Carries the underlying error rendered to text (one batch
    /// outcome fans out to many flush waiters).
    #[error("write batch failed: {0}")]
    WriteBatchFailed(String),
}

pub type Result<T> = std::result::Result<T, ChatStoreError>;

/// A diesel error as the storage error this crate reports.
///
/// Public rather than `pub(crate)` because the integration tests are a
/// separate crate and issue their own statements: `map_err(db_err)` was
/// written out as its expansion thirteen times over there, which is one
/// spelling of the boxed variant per site to keep in step.
///
/// Hidden from the docs: it is reachable rather than offered, and the surface
/// an embedder is meant to read is `ChatStoreError`.
#[doc(hidden)]
pub fn db_err(e: diesel::result::Error) -> StoreError {
    StoreError::Database(Box::new(e))
}
