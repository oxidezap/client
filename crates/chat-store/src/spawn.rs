//! Where the writer task runs.
//!
//! The store owns exactly one long-lived task — the writer queue, which is
//! what makes an ack unable to outrun the write that created it — and one
//! task is the whole of this crate's relationship with an executor. Which
//! executor that is differs, and it is not this crate's business to say:
//! `oxidezap-platform` is the one place in the workspace that answers it.
//!
//! A desktop store runs inside the session's Tokio runtime and hands the task
//! to it. A page has no runtime at all: `tokio::spawn` there does not fail to
//! find a reactor in some recoverable way, it panics — "there is no reactor
//! running" — and takes the store's opening with it.
//!
//! The bound is the difference, and it is the same one `oxidezap-session`
//! draws for the same reason: a work-stealing runtime may move a task between
//! threads and so requires `Send`; a browser's event loop moves nothing
//! anywhere and must not require it. That bound is `MaybeSend`, named where
//! the spawn is.
//!
//! Nothing is returned, on either platform: the writer loop ends when its
//! channel closes, which happens when the last handle to the store is
//! dropped, so there is no handle anybody would hold and no cancellation
//! anybody would ask for.

pub(crate) use oxidezap_platform::spawn;
