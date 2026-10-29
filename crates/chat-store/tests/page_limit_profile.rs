//! Linux release workload for pooled reads, alias folding and quote hydration.
//! Synthetic setup and warmup are excluded. Rust allocation traffic is measured
//! separately from timing; SQLite's C allocator is outside GlobalAlloc.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)]
// The profiling allocator and Linux CPU clock wrap their documented FFI only.
#![allow(unsafe_code)]

mod common;
use common::*;
use oxidezap_chat_store::StoredMessage;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static COUNT: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNT.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn cpu_ns() -> u64 {
    #[repr(C)]
    struct Timespec {
        seconds: std::ffi::c_long,
        nanos: std::ffi::c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: std::ffi::c_int, time: *mut Timespec) -> std::ffi::c_int;
    }
    let mut time = Timespec {
        seconds: 0,
        nanos: 0,
    };
    // Linux CLOCK_PROCESS_CPUTIME_ID includes all pool/runtime threads.
    assert_eq!(unsafe { clock_gettime(2, &mut time) }, 0);
    time.seconds as u64 * 1_000_000_000 + time.nanos as u64
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn reply(chat: &str, parent: &str) -> wa::Message {
    let mut context = wa::ContextInfo::default();
    context.stanza_id = Some(parent.into());
    context.participant = Some(chat.into());
    context.quoted_message = MessageField::some(wa::Message::text("synthetic body"));
    let mut text = wa::message::ExtendedTextMessage::default();
    text.text = Some("synthetic reply".into());
    text.context_info = MessageField::some(context);
    let mut message = wa::Message::default();
    message.extended_text_message = MessageField::some(text);
    message
}

fn reaction(chat: &str, target: &str) -> wa::Message {
    let mut key = wa::MessageKey::default();
    key.remote_jid = Some(chat.into());
    key.id = Some(target.into());
    key.from_me = Some(false);
    let mut reaction = wa::message::ReactionMessage::default();
    reaction.key = MessageField::some(key);
    reaction.text = Some("👍".into());
    let mut message = wa::Message::default();
    message.reaction_message = MessageField::some(reaction);
    message
}

const CHATS: usize = 24;
const MESSAGES: usize = 80;

struct Workload {
    store: Arc<ChatStore>,
    chats: Vec<Jid>,
    before: Vec<MessageCursor>,
    after: Vec<MessageCursor>,
    ids: Vec<Vec<String>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ResultShape {
    rows: u64,
    checksum: u64,
    quotes: u64,
    reactions: u64,
}

impl ResultShape {
    fn add(&mut self, rows: &[StoredMessage]) {
        self.rows += rows.len() as u64;
        for row in rows {
            assert_ne!(row.text.as_deref(), Some("other account"));
            self.checksum = self
                .checksum
                .wrapping_add(row.seq as u64)
                .wrapping_add(row.id.bytes().map(u64::from).sum::<u64>());
            if let Some(quote) = row
                .message
                .as_ref()
                .and_then(|m| m.extended_text_message.as_option())
                .and_then(|m| m.context_info.as_option())
                .and_then(|m| m.quoted_message.as_option())
            {
                assert_eq!(quote.conversation.as_deref(), Some("synthetic body"));
                self.quotes += 1;
            }
        }
    }
}

impl Workload {
    async fn read(&self, case: &str) -> ResultShape {
        let mut shape = ResultShape::default();
        if case == "attach" || case == "history" {
            let limit = if case == "attach" { 8 } else { 50 };
            let pages = self
                .store
                .pages(
                    self.chats
                        .iter()
                        .map(|chat| (chat.clone(), limit))
                        .collect(),
                )
                .await
                .unwrap();
            assert_eq!(pages.len(), CHATS);
            for page in pages.values() {
                assert_eq!(page.len(), limit as usize);
                shape.add(page);
            }
        } else if case != "reactions" {
            for (i, chat) in self.chats.iter().enumerate() {
                let page = match case {
                    "backward" => self
                        .store
                        .messages(chat, Some(self.before[i].clone()), 50)
                        .await
                        .unwrap(),
                    "forward" => self
                        .store
                        .messages_after(chat, self.after[i].clone(), 50)
                        .await
                        .unwrap(),
                    _ => panic!("unknown case {case}"),
                };
                assert_eq!(page.len(), 50);
                shape.add(&page);
            }
        }
        if case == "history" || case == "reactions" {
            for (chat, ids) in self.chats.iter().zip(&self.ids) {
                let reactions = self.store.reactions_for(chat, ids.clone()).await.unwrap();
                shape.reactions += reactions.values().map(|r| r.len() as u64).sum::<u64>();
            }
        }
        shape
    }
}

async fn seed() -> (SqliteStore, Workload) {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
    let (db, store) = test_store().await;
    let mut chats = Vec::new();
    let mut events = Vec::new();
    for c in 0..CHATS {
        let chat = format!("55990000{c:04}@s.whatsapp.net");
        chats.push(jid(&chat));
        for m in 0..MESSAGES {
            let id = format!("M-{c}-{m}");
            let message = if m % 10 == 9 {
                reply(&chat, &format!("M-{c}-{}", m - 1))
            } else {
                wa::Message::text("synthetic body")
            };
            events.push(message_event(
                message,
                incoming_info(&chat, &chat, &id, 1_700_000_000 + (m / 4) as i64),
            ));
            if m % 10 == 0 {
                events.push(message_event(
                    reaction(&chat, &id),
                    incoming_info(&chat, &chat, &format!("R-{c}-{m}"), 1_700_000_100),
                ));
            }
        }
    }
    for batch in events.chunks(128) {
        feed(&store, batch.iter().cloned()).await;
    }
    for c in 0..CHATS {
        db.put_lid_mapping(&LidPnMappingEntry {
            lid: format!("77000000{c:04}"),
            phone_number: format!("55990000{c:04}"),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "synthetic".into(),
        })
        .await
        .unwrap();
    }
    db.database().provision_device(2).await.unwrap();
    db.shared().run(|conn| {
        // Legacy copies are intentionally inserted after writer reconciliation.
        // They force read-time author folding across proven alias keys.
        diesel::sql_query("INSERT INTO messages(device_id,chat_jid,msg_id,sender_jid,from_me,timestamp_ms,kind,text_content,proto,proto_codec,status) \
            SELECT m.device_id,l.lid||'@lid',m.msg_id,l.lid||'@lid',m.from_me,m.timestamp_ms,m.kind,m.text_content,m.proto,m.proto_codec,m.status \
            FROM messages m JOIN lid_pn_mapping l ON l.device_id=m.device_id AND m.chat_jid=l.phone_number||'@s.whatsapp.net' \
            WHERE m.device_id=1 AND m.msg_id GLOB '*5'").execute(conn).map_err(db_err)?;
        diesel::sql_query("INSERT INTO messages(device_id,chat_jid,msg_id,sender_jid,from_me,timestamp_ms,kind,text_content) \
            SELECT 2,chat_jid,msg_id,sender_jid,from_me,timestamp_ms,kind,'other account' FROM messages WHERE device_id=1")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut ids = Vec::new();
    for chat in &chats {
        let mut page = store.messages(chat, None, MESSAGES as i64).await.unwrap();
        assert_eq!(page.len(), MESSAGES, "read-time duplicates must fold");
        page.sort_by_key(|m| (m.timestamp, m.seq));
        before.push(MessageCursor {
            timestamp_ms: page[50].timestamp.timestamp_millis(),
            seq: page[50].seq,
        });
        after.push(MessageCursor {
            timestamp_ms: page[29].timestamp.timestamp_millis(),
            seq: page[29].seq,
        });
        ids.push(page[30..].iter().map(|m| m.id.clone()).collect());
    }
    (
        db,
        Workload {
            store,
            chats,
            before,
            after,
            ids,
        },
    )
}

#[tokio::test]
#[ignore = "Linux release pooled hydration measurement"]
async fn pooled_page_limit_workload() {
    let case = std::env::var("PAGE_LIMIT_CASE").unwrap_or_else(|_| "attach".into());
    let iterations: usize = std::env::var("PAGE_LIMIT_ITERATIONS")
        .unwrap_or_else(|_| "64".into())
        .parse()
        .unwrap();
    let allocations = std::env::var("PAGE_LIMIT_ALLOC").is_ok_and(|v| v == "1");
    let (_db, workload) = seed().await;
    let expected = workload.read(&case).await;
    assert_eq!(
        expected.rows,
        if case == "reactions" {
            0
        } else if case == "attach" {
            CHATS as u64 * 8
        } else {
            CHATS as u64 * 50
        }
    );
    if case != "reactions" {
        assert!(expected.quotes > 0);
    }
    if case == "history" || case == "reactions" {
        assert_eq!(expected.reactions, CHATS as u64 * 5);
    }
    for _ in 0..3 {
        assert_eq!(workload.read(&case).await, expected);
    }
    let rss_before = rss_kib();
    CALLS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    let cpu_before = cpu_ns();
    COUNT.store(allocations, Ordering::Relaxed);
    let start = wacore::time::Instant::now();
    for _ in 0..iterations {
        assert_eq!(workload.read(&case).await, expected);
    }
    let elapsed_ns = start.elapsed().as_nanos();
    COUNT.store(false, Ordering::Relaxed);
    let cpu_ns = cpu_ns() - cpu_before;
    let calls = CALLS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let rss_after = rss_kib();
    println!(
        "PAGE_LIMIT elapsed_ns={elapsed_ns} cpu_ns={cpu_ns} alloc_calls={calls} alloc_bytes={bytes} rss_before_kib={rss_before} rss_after_kib={rss_after} rows={} checksum={} quotes={} reactions={}",
        expected.rows, expected.checksum, expected.quotes, expected.reactions
    );
}
