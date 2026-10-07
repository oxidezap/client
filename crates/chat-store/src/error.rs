use thiserror::Error;
use wacore::store::error::StoreError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChatStoreError {
    #[error("storage error")]
    Store(#[from] StoreError),

    #[error("invalid full-text search query")]
    InvalidSearchQuery,

    #[error("message id is ambiguous; query the chat page for sender identity")]
    AmbiguousMessageId,

    /// A writer barrier failed. Rolled-back accepted writes remain in memory
    /// for a later flush, unless the writer was closed. May also report an
    /// admission rejection or a post-commit backend durability failure.
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
