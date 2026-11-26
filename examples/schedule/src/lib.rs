use chrono::{DateTime, Utc};
use oxidezap_plugin::{
    Action, Caps, Declared, Event, Kinds, Outcome, Setup, Which, now_ms, plugin, ui,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

plugin!(init = setup, event = handle, panic = own);

#[derive(Clone)]
struct EditDraft {
    id: u64,
    text: String,
    when: String,
}

thread_local! {
    static EDIT_DRAFT: RefCell<Option<EditDraft>> = const { RefCell::new(None) };
    static REPEAT_DAILY: RefCell<bool> = const { RefCell::new(false) };
}

fn read_edit_draft() -> Option<EditDraft> {
    EDIT_DRAFT.with(|cell| cell.borrow().clone())
}

fn write_edit_draft(draft: Option<EditDraft>) {
    EDIT_DRAFT.with(|cell| *cell.borrow_mut() = draft);
}

fn read_repeat_daily() -> bool {
    REPEAT_DAILY.with(|cell| *cell.borrow())
}

fn write_repeat_daily(repeat: bool) {
    REPEAT_DAILY.with(|cell| *cell.borrow_mut() = repeat);
}

const STORE: &str = "messages";
const TICK: i64 = 1;
const MAX_QUEUE_BYTES: usize = 8192;
const MAX_JOBS: usize = 8;
const MAX_MESSAGE: usize = 512;
const POLL_MS: i64 = 5_000;

#[derive(Clone, Serialize, Deserialize)]
struct Job {
    id: u64,
    jid: String,
    text: String,
    at: i64,
    attempting: bool,
    #[serde(default)]
    repeat_daily: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Queue {
    last_id: u64,
    jobs: Vec<Job>,
}

#[derive(Deserialize)]
struct Form {
    message: String,
    when: String,
}

fn setup(p: Setup) -> impl Declared {
    write_edit_draft(None);
    write_repeat_daily(false);
    let declared = p
        .name("Scheduled messages")
        .subscribe(Kinds::NONE)
        .capabilities(Caps::UI | Caps::STORAGE | Caps::TIMERS | Caps::SEND);
    draw(&read_queue(), "");
    oxidezap_plugin::after(100, TICK);
    declared
}

fn read_queue() -> Result<Queue, &'static str> {
    let stored = oxidezap_plugin::get::<MAX_QUEUE_BYTES>(STORE);
    let Some(raw) = stored.whole().filter(|_| !stored.refused()) else {
        return if stored.refused() || stored.present() {
            Err("The schedule could not be read.")
        } else {
            Ok(Queue::default())
        };
    };
    if !stored.present() {
        return Ok(Queue::default());
    }
    serde_json::from_str(raw).map_err(|_| "The stored schedule could not be read.")
}

fn save(queue: &Queue, durable: bool) -> Result<(), &'static str> {
    let raw = serde_json::to_string(queue).map_err(|_| "The schedule could not be saved.")?;
    if raw.len() > MAX_QUEUE_BYTES {
        return Err("The schedule is past its storage limit.");
    }
    let outcome = if durable {
        oxidezap_plugin::set_durable(STORE, &raw)
    } else {
        oxidezap_plugin::set(STORE, &raw)
    };
    match outcome {
        Outcome::Accepted => Ok(()),
        Outcome::Unconfirmed => Err(
            "The schedule changed, but storage could not confirm the write. Check the list before retrying.",
        ),
        _ => Err("The schedule could not be saved. Try again in a moment."),
    }
}

fn chat_supported(jid: &str) -> bool {
    jid.rsplit_once('@').is_some_and(|(user, server)| {
        !user.is_empty()
            && user.bytes().all(|b| b.is_ascii_digit() || b == b'-')
            && matches!(server, "s.whatsapp.net" | "lid" | "g.us")
    })
}

fn deadline(when: &str, now: i64) -> Option<i64> {
    let when = when.trim();
    let at = if let Ok(minutes) = when.parse::<i64>() {
        now.checked_add(minutes.checked_mul(60_000)?)?
    } else {
        DateTime::parse_from_rfc3339(when).ok()?.timestamp_millis()
    };
    (at > now && at.saturating_sub(now) <= 366 * 86_400_000).then_some(at)
}

fn edit_queue(action: Action<'_>, queue: &mut Queue) -> Result<&'static str, &'static str> {
    let id = action.id();
    let chat = action.chat();
    let jid = chat.as_str();
    if let Some(id) = id
        .as_str()
        .strip_prefix("cancel-")
        .and_then(|id| id.parse::<u64>().ok())
    {
        queue.jobs.retain(|job| job.id != id);
        if read_edit_draft().is_some_and(|d| d.id == id) {
            write_edit_draft(None);
        }
        save(queue, true)?;
        return Ok("Removed from the schedule.");
    }
    if let Some(id) = id
        .as_str()
        .strip_prefix("edit-")
        .and_then(|id| id.parse::<u64>().ok())
    {
        let Some(job) = queue.jobs.iter().find(|j| j.id == id && !j.attempting) else {
            return Err("Scheduled message is no longer editable.");
        };
        let when = DateTime::<Utc>::from_timestamp_millis(job.at)
            .map(|time| time.to_rfc3339())
            .unwrap_or_default();
        write_edit_draft(Some(EditDraft {
            id,
            text: job.text.clone(),
            when,
        }));
        return Ok("");
    }
    if let Some(target_id) = id
        .as_str()
        .strip_prefix("edit-message-")
        .and_then(|s| s.parse::<u64>().ok())
    {
        let value = action.value_into::<4096>();
        if let Some(text) = value.whole()
            && let Some(mut d) = read_edit_draft().filter(|d| d.id == target_id)
        {
            d.text = text.to_owned();
            write_edit_draft(Some(d));
        }
        return Ok("");
    }
    if let Some(target_id) = id
        .as_str()
        .strip_prefix("edit-when-")
        .and_then(|s| s.parse::<u64>().ok())
    {
        let value = action.value_into::<256>();
        if let Some(when) = value.whole()
            && let Some(mut d) = read_edit_draft().filter(|d| d.id == target_id)
        {
            d.when = when.to_owned();
            write_edit_draft(Some(d));
        }
        return Ok("");
    }
    if let Some(target_id) = id
        .as_str()
        .strip_prefix("discard-edit-")
        .and_then(|s| s.parse::<u64>().ok())
    {
        if read_edit_draft().is_some_and(|d| d.id == target_id) {
            write_edit_draft(None);
        }
        return Ok("");
    }
    if let Some(target_id) = id
        .as_str()
        .strip_prefix("save-edit-")
        .and_then(|s| s.parse::<u64>().ok())
    {
        let mut draft_val = read_edit_draft()
            .filter(|d| d.id == target_id)
            .ok_or("This edit session is no longer active.")?;
        // In PluginSlot::Composer, button presses carry the current form values
        // as a JSON object keyed by widget id; in Settings, fields commit as
        // individual actions into `EDIT_DRAFT`.
        let value = action.value_into::<4096>();
        if let Some(raw) = value.whole().filter(|s| !s.trim().is_empty())
            && let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw)
        {
            let msg_key = format!("edit-message-{target_id}");
            let when_key = format!("edit-when-{target_id}");
            if let Some(text) = map.get(&msg_key).and_then(|v| v.as_str()) {
                draft_val.text = text.to_owned();
            }
            if let Some(when) = map.get(&when_key).and_then(|v| v.as_str()) {
                draft_val.when = when.to_owned();
            }
        }
        if draft_val.text.trim().is_empty() || draft_val.text.len() > MAX_MESSAGE {
            return Err("Enter a message of up to 512 bytes.");
        }
        let at =
            deadline(&draft_val.when, now_ms()).ok_or("Enter a future time within one year.")?;
        let Some(job) = queue
            .jobs
            .iter_mut()
            .find(|j| j.id == draft_val.id && !j.attempting)
        else {
            return Err("Scheduled message is no longer editable.");
        };
        job.text = draft_val.text;
        job.at = at;
        save(queue, true)?;
        write_edit_draft(None);
        return Ok("Scheduled message updated.");
    }
    if id.as_str() == "repeat_daily" {
        let on = action.value_into::<8>().as_str() == "1";
        write_repeat_daily(on);
        return Ok("");
    }
    if !chat_supported(jid) {
        return Err("Open a direct conversation or group to schedule a message.");
    }
    if id.as_str() != "schedule" {
        return Ok("");
    }
    if queue.jobs.len() >= MAX_JOBS {
        return Err("The schedule is full.");
    }
    let value = action.value_into::<4096>();
    let form: Form = value
        .whole()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .ok_or("The form could not be read.")?;
    if form.message.trim().is_empty() || form.message.len() > MAX_MESSAGE {
        return Err("Enter a message of up to 512 bytes.");
    }
    let now = now_ms();
    let at = deadline(&form.when, now).ok_or("Enter a future time within one year.")?;
    if queue
        .jobs
        .iter()
        .any(|job| job.jid == jid && job.text == form.message && job.at.abs_diff(at) < 5_000)
    {
        return Err("This message is already in the schedule.");
    }
    queue.last_id = queue
        .last_id
        .checked_add(1)
        .ok_or("The schedule is full.")?;
    queue.jobs.push(Job {
        id: queue.last_id,
        jid: jid.into(),
        text: form.message,
        at,
        attempting: false,
        repeat_daily: read_repeat_daily(),
    });
    save(queue, true)?;
    write_repeat_daily(false);
    Ok("Message scheduled.")
}

fn tick(queue: &mut Queue) -> Result<bool, &'static str> {
    let now = now_ms();
    let Some(index) = queue
        .jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| !job.attempting && job.at <= now)
        .min_by_key(|(_, job)| (job.at, job.id))
        .map(|(index, _)| index)
    else {
        return Ok(false);
    };
    // A restart between this write and the send cannot replay an uncertain send.
    queue.jobs[index].attempting = true;
    if let Err(error) = save(queue, true) {
        // No send was attempted. Undo a marker that may have replaced the
        // file so a later timer can try again.
        queue.jobs[index].attempting = false;
        save(queue, false)?;
        return Err(error);
    }
    let job = &queue.jobs[index];
    match oxidezap_plugin::send_text(&job.jid, &job.text) {
        Outcome::Accepted => {
            const DAY_MS: i64 = 86_400_000;
            if queue.jobs[index].repeat_daily {
                let mut next = queue.jobs[index].at;
                while next <= now {
                    let Some(advanced) = next.checked_add(DAY_MS) else {
                        queue.jobs.remove(index);
                        save(queue, false)?;
                        return Ok(true);
                    };
                    next = advanced;
                }
                queue.jobs[index].at = next;
                queue.jobs[index].attempting = false;
            } else {
                queue.jobs.remove(index);
            }
        }
        Outcome::NoSession | Outcome::Denied | Outcome::Refused => {
            queue.jobs[index].attempting = false;
        }
        Outcome::Invalid | Outcome::State | Outcome::Unconfirmed => return Ok(true),
    }
    save(queue, false)?;
    Ok(true)
}

fn handle(event: &Event) {
    match event.which() {
        Which::Action(action) => {
            let queue = read_queue();
            let mut proposed = queue.clone();
            match proposed.as_mut().map(|queue| edit_queue(action, queue)) {
                Ok(Ok(status)) => draw(&proposed, status),
                Ok(Err(error)) => draw(&read_queue(), error),
                Err(error) => draw(&queue, error),
            }
        }
        Which::Timer(timer) if timer.token() == TICK => {
            let mut queue = read_queue();
            match queue.as_mut().map(tick) {
                Ok(Ok(true)) => draw(&queue, ""),
                Ok(Err(error)) => draw(&read_queue(), error),
                _ => {}
            }
            oxidezap_plugin::after(POLL_MS, TICK);
        }
        _ => {}
    }
}

fn draw_scheduled_list(
    section: &mut ui::Group<'_, '_>,
    queue: &Result<Queue, &'static str>,
    editing: Option<&EditDraft>,
    notice: &str,
) {
    if !notice.is_empty() {
        section.label(notice);
    }
    let queue = match queue {
        Ok(queue) => queue,
        Err(error) => {
            section.label(error);
            return;
        }
    };
    if queue.jobs.is_empty() {
        section.label("No scheduled messages.");
    }
    for job in &queue.jobs {
        let when = DateTime::<Utc>::from_timestamp_millis(job.at)
            .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        let recurrence = if job.repeat_daily { " · daily" } else { "" };
        section.label(&format!("{} · {}{}", job.jid, when, recurrence));
        section.label(&job.text);
        if !job.attempting {
            if let Some(draft) = editing.as_ref().filter(|d| d.id == job.id) {
                section.field(
                    &format!("edit-message-{}", job.id),
                    "Message",
                    &draft.text,
                    true,
                );
                section.datetime(
                    &format!("edit-when-{}", job.id),
                    "Send time",
                    &draft.when,
                    true,
                );
                section.row(|row| {
                    row.button(&format!("save-edit-{}", job.id), "Save");
                    row.button(&format!("discard-edit-{}", job.id), "Discard");
                });
            } else {
                section.button(&format!("edit-{}", job.id), "Edit scheduled message");
            }
        }
        let label = if job.attempting {
            "Dismiss unconfirmed send"
        } else {
            "Cancel scheduled message"
        };
        section.button(&format!("cancel-{}", job.id), label);
    }
}

fn draw(queue: &Result<Queue, &'static str>, notice: &str) {
    let editing = read_edit_draft();
    let repeat_daily = read_repeat_daily();
    ui::publish::<32768>(|ui| {
        ui.section(ui::slot::COMPOSER, "Schedule a message", |section| {
            section.datetime("when", "Send time", "", true);
            section.toggle("repeat_daily", "Repeat daily", repeat_daily, true);
            section.field("message", "Type a message (up to 512 bytes)", "", true);
            section.button("schedule", "Schedule Send");
            if !notice.is_empty() {
                section.label(notice);
            }
        });
        ui.section(ui::slot::COMPOSER, "Scheduled messages", |section| {
            draw_scheduled_list(section, queue, editing.as_ref(), "");
        });
        ui.section(ui::slot::SETTINGS, "Scheduled messages", |section| {
            draw_scheduled_list(section, queue, editing.as_ref(), notice);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_plugin::{
        abi,
        testing::{Event as In, Host},
    };

    const CHAT: &str = "15550100000@s.whatsapp.net";

    fn schedule(host: &mut Host, chat: &str, when: &str) {
        let value = serde_json::json!({"message": "hello", "when": when}).to_string();
        host.deliver(
            In::action("schedule", &value).str(abi::fields::CHAT_JID, chat),
            handle,
        );
    }

    #[test]
    fn persisted_job_is_sent_once_at_or_after_its_deadline() {
        let mut host = Host::new();
        host.init(setup);
        assert!(
            host.ui()
                .unwrap()
                .unwrap()
                .iter()
                .any(|node| node.slot == ui::slot::COMPOSER)
        );
        host.set_now_ms(1_000);
        schedule(&mut host, CHAT, "1");
        host.deliver(In::timer(TICK), handle);
        assert!(host.sent().is_empty());

        let saved = host.stored(STORE).unwrap();
        let mut restarted = Host::new();
        restarted.store(STORE, &saved);
        restarted.init(setup);
        restarted.set_now_ms(61_000);
        restarted.deliver(In::timer(TICK), handle);
        restarted.deliver(In::timer(TICK), handle);
        assert_eq!(restarted.sent(), [(CHAT.to_owned(), "hello".to_owned())]);
    }

    #[test]
    fn invalid_destinations_and_cancelled_jobs_do_not_send() {
        let mut host = Host::new();
        host.init(setup);
        schedule(&mut host, "status@broadcast", "1");
        assert!(host.stored(STORE).is_none());
        schedule(&mut host, CHAT, "1");
        host.deliver(In::action("cancel-1", ""), handle);
        host.set_now_ms(60_000);
        host.deliver(In::timer(TICK), handle);
        assert!(host.sent().is_empty());
    }

    #[test]
    fn an_unconfirmed_marker_is_retried_without_sending_early() {
        let mut host = Host::new();
        host.init(setup);
        schedule(&mut host, CHAT, "1");
        host.set_now_ms(60_000);
        host.unconfirm_next_durable_write();
        host.deliver(In::timer(TICK), handle);
        assert!(host.sent().is_empty());
        let stored: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert!(!stored.jobs[0].attempting);
        host.deliver(In::timer(TICK), handle);
        assert_eq!(host.sent().len(), 1);
    }

    #[test]
    fn a_restart_does_not_replay_an_unconfirmed_attempt() {
        let mut host = Host::new();
        let queue = Queue {
            last_id: 1,
            jobs: vec![Job {
                id: 1,
                jid: CHAT.into(),
                text: "hello".into(),
                at: 100,
                attempting: true,
                repeat_daily: false,
            }],
        };
        host.store(STORE, &serde_json::to_string(&queue).unwrap());
        host.init(setup);
        host.set_now_ms(100);
        host.deliver(In::timer(TICK), handle);
        assert!(host.sent().is_empty());
    }

    #[test]
    fn an_oversized_serialized_queue_does_not_publish_an_unsaved_job() {
        let mut host = Host::new();
        host.init(setup);
        let value = serde_json::json!({"message": "\0".repeat(512), "when": "1"}).to_string();
        for n in 0..3 {
            host.set_now_ms(n * 60_000);
            host.deliver(
                In::action("schedule", &value).str(abi::fields::CHAT_JID, CHAT),
                handle,
            );
        }
        let saved: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert_eq!(saved.jobs.len(), 2);
        let tree = host.ui().unwrap().unwrap();
        assert_eq!(
            tree.iter()
                .filter(|node| node.slot == ui::slot::SETTINGS)
                .flat_map(|node| &node.children)
                .filter(|node| node.id.starts_with("cancel-"))
                .count(),
            2
        );
    }

    #[test]
    fn parses_offsets_without_assuming_the_daemon_timezone() {
        assert_eq!(deadline("1970-01-01T00:01:00Z", 0), Some(60_000));
        assert_eq!(deadline("1970-01-01T01:01:00+01:00", 0), Some(60_000));
        assert_eq!(deadline("1970-01-01T00:01:00", 0), None);
        assert_eq!(deadline("-1", 0), None);
    }

    #[test]
    fn composer_uses_datetime_picker_and_jobs_can_be_edited() {
        let mut host = Host::new();
        host.init(setup);
        let tree = host.ui().unwrap().unwrap();
        let composer = tree
            .iter()
            .find(|node| node.slot == ui::slot::COMPOSER)
            .expect("composer root");
        assert!(composer.children.iter().any(|node| node.kind
            == oxidezap_plugin::abi::ui::kind::DATE_TIME
            && node.id == "when"));

        host.set_now_ms(1_000);
        schedule(&mut host, CHAT, "10");
        host.deliver(In::action("edit-1", ""), handle);
        host.deliver(In::action("edit-message-1", "updated hello"), handle);
        host.deliver(In::action("edit-when-1", "1970-01-01T00:05:00Z"), handle);
        host.deliver(In::action("save-edit-1", ""), handle);

        let saved: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert_eq!(saved.jobs.len(), 1);
        assert_eq!(saved.jobs[0].text, "updated hello");
        assert_eq!(saved.jobs[0].at, 300_000);
    }

    #[test]
    fn repeat_daily_reschedules_job_for_next_day() {
        let mut host = Host::new();
        host.init(setup);
        host.set_now_ms(1_000);
        host.deliver(
            In::action("repeat_daily", "1").str(abi::fields::CHAT_JID, CHAT),
            handle,
        );
        schedule(&mut host, CHAT, "1");
        host.set_now_ms(61_000);
        host.deliver(In::timer(TICK), handle);
        assert_eq!(host.sent(), [(CHAT.to_owned(), "hello".to_owned())]);

        let saved: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert_eq!(saved.jobs.len(), 1);
        assert!(saved.jobs[0].repeat_daily);
        assert!(!saved.jobs[0].attempting);
        assert_eq!(saved.jobs[0].at, 61_000 + 86_400_000);

        // Simulating multiple offline days: only one send fires and the next
        // deadline skips strictly past `now`.
        let four_days_later = 61_000 + 4 * 86_400_000 + 5_000;
        host.set_now_ms(four_days_later);
        host.deliver(In::timer(TICK), handle);
        host.deliver(In::timer(TICK), handle);
        assert_eq!(host.sent().len(), 2);
        let saved: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert!(saved.jobs[0].at > four_days_later);
    }

    #[test]
    fn composer_form_edit_updates_job() {
        let mut host = Host::new();
        host.init(setup);
        host.set_now_ms(1_000);
        schedule(&mut host, CHAT, "10");
        host.deliver(In::action("edit-1", ""), handle);
        let form = serde_json::json!({
            "edit-message-1": "composer updated",
            "edit-when-1": "1970-01-01T00:06:00Z",
        })
        .to_string();
        host.deliver(
            In::action("save-edit-1", &form).str(abi::fields::CHAT_JID, CHAT),
            handle,
        );
        let saved: Queue = serde_json::from_str(&host.stored(STORE).unwrap()).unwrap();
        assert_eq!(saved.jobs[0].text, "composer updated");
        assert_eq!(saved.jobs[0].at, 360_000);
    }
}
