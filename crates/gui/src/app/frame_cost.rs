//! What one frame of the conversation allocates.
//!
//! A stopwatch rather than an assertion, in the shape of
//! `history_hydration_costs`: it prints, and it asserts only the thing worth
//! keeping honest — that the per-frame path allocates in proportion to what is
//! *on screen* rather than to the size of the conversation behind it.
//!
//! Run it with
//! `cargo test -p oxidezap-gui -- --ignored --nocapture frame_cost`.
//!
//! Why an allocation count and not a duration: the number that matters is on
//! the web, where the allocator is `dlmalloc` and every one of these costs
//! several times what it costs here. A wall clock measured on this machine
//! would say very little about the machine that has the problem; a count is
//! the same count in both.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Allocations made by *this* thread.
    ///
    /// Per thread rather than global because the test harness runs tests in
    /// parallel, and a global tally read either side of one closure counts
    /// whatever every other test was doing meanwhile — which reads as the
    /// measured code allocating, at a number that changes between runs.
    ///
    /// `const`-initialized so the first access from inside `alloc` cannot
    /// itself allocate.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// Bump this thread's tally, ignoring allocations made while TLS is being
/// torn down — there is nothing left to attribute them to.
fn count() {
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

/// `System`, plus a tally.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's contract for `GlobalAlloc::alloc`, passed
        // through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How many allocations `body` made.
fn allocations(body: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    body();
    ALLOCATIONS.with(Cell::get) - before
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use oxidezap_core::{Chat, ChatMessage};

    use super::allocations;
    use crate::app::messages::MessageListCache;

    /// A conversation of `count` messages, shaped like a real one: text, a
    /// sender, an id and a timestamp, which is what every row draws from.
    fn conversation(count: usize) -> Chat {
        let mut chat = Chat::new("5511900000000@s.whatsapp.net".to_string());
        for n in 0..count {
            chat.messages.push(ChatMessage::new_incoming(
                format!("M-{n}"),
                chat.jid.clone(),
                "uma mensagem de tamanho bastante comum".to_string(),
            ));
        }
        chat
    }

    /// The two things a frame does with the selected conversation: take it out
    /// of the app, and read the timeline's cache.
    ///
    /// Both were proportional to the whole conversation. Neither is now, and
    /// that is the assertion: the numbers below are a machine's, the *shape*
    /// is the contract.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn frame_cost_does_not_follow_the_conversation() {
        for count in [50usize, 500, 5_000] {
            let chat = Arc::new(conversation(count));

            // What `views::chat` does per frame to get at the selected chat.
            let taking = allocations(|| {
                let taken = Arc::clone(&chat);
                std::hint::black_box(&taken);
            });

            // And what it used to do, kept here as the comparison: a `Chat`
            // is its messages, and each of those is four `String`s, a
            // reaction map, a quote and a media handle.
            let by_value = allocations(|| {
                let copied: oxidezap_core::Chat = (*chat).clone();
                std::hint::black_box(&copied);
            });

            // And what the timeline's cache costs when it hits, which is every
            // frame in which nothing about the conversation changed.
            let cache = MessageListCache::new(&chat.messages, false, None);
            let reading = allocations(|| {
                let hit = cache.clone();
                std::hint::black_box(&hit);
            });

            println!(
                "{count:>5} messages: taking the chat {taking:>2} allocations \
                 (was {by_value:>6} by value), reading the cached rows {reading:>2}"
            );

            assert!(
                taking <= 1,
                "taking the selected chat allocated {taking} times for {count} messages; \
                 it is meant to be a refcount"
            );
            assert!(
                reading <= 1,
                "reading the cached rows allocated {reading} times for {count} messages"
            );
        }
    }

    /// The ids a bubble draws under are formatted when the rows are built, so
    /// the count is the conversation's once and not the viewport's per frame.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn bubble_ids_are_built_once() {
        let chat = conversation(500);
        let cache = MessageListCache::new(&chat.messages, false, None);

        let handing_out = allocations(|| {
            // Twenty rows is a viewport; this is what the list does per frame.
            for row in cache.ids.iter().take(20) {
                std::hint::black_box(row.clone());
            }
        });
        println!("20 visible rows: {handing_out} allocations for their element ids");
        assert_eq!(
            handing_out, 0,
            "a `SharedString` clone is a refcount; formatting one is not"
        );
    }
}
