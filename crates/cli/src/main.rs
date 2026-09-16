//! OxideZap CLI: A lightweight, scriptable command-line interface for WhatsApp.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod args;
mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use args::{Commands, OxidezapCli};
use output::{OutputMode, print_error, print_event, print_result};
use oxidezap_ipc::IpcClient;
use oxidezap_wire::envelope::CURRENT_PROTOCOL_VERSION;
use oxidezap_wire::request::ClientRequest;
use oxidezap_wire::response::DaemonResponse;

fn main() -> ExitCode {
    let cli = OxidezapCli::parse();

    let output_mode = if cli.json {
        OutputMode::Json
    } else if cli.events {
        OutputMode::Events
    } else {
        OutputMode::Human
    };

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            eprintln!("No command specified. Run `oxidezap-cli --help` for available commands.");
            return ExitCode::from(1);
        }
    };

    // Commands that execute locally without requiring daemon connection
    match command {
        // Selecting an account is choosing a socket, so it needs no
        // daemon. Eval the line to apply it:
        // `eval $(oxidezap-cli accounts use work)`.
        Commands::Accounts(ref accounts)
            if matches!(accounts.command, Some(args::AccountsSubcommand::Use(_))) =>
        {
            if let Some(args::AccountsSubcommand::Use(u)) = &accounts.command {
                if u.id == "default" {
                    println!("unset OXIDEZAP_ACCOUNT");
                } else {
                    println!("export OXIDEZAP_ACCOUNT={}", u.id);
                }
            }
            return ExitCode::SUCCESS;
        }
        Commands::Completion(comp) => {
            let shell = match comp.shell.as_str() {
                "bash" => usage::complete::Shell::Bash,
                "zsh" => usage::complete::Shell::Zsh,
                _ => usage::complete::Shell::Fish,
            };
            print!("{}", OxidezapCli::completion_script(shell));
            return ExitCode::SUCCESS;
        }
        Commands::Mcp(_mcp) => {
            print!("{}", OxidezapCli::to_kdl());
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    // A named account profile owns its socket; without one this is the
    // default profile on the historic path. An explicit socket always wins.
    // Set before connecting, so the endpoint derived below agrees.
    if let Some(account) = cli.account.as_deref()
        && std::env::var_os("OXIDEZAP_ACCOUNT").is_none()
    {
        // Single-threaded startup, before any thread exists: no thread can
        // observe the environment changing under it.
        unsafe {
            std::env::set_var("OXIDEZAP_ACCOUNT", account);
        }
    }

    // Connect to daemon
    let mut client = match cli.socket.as_deref() {
        Some(socket_path) => match IpcClient::connect_at(&PathBuf::from(socket_path)) {
            Ok(c) => c,
            Err(e) => {
                print_error(
                    output_mode,
                    "socket_connect_failed",
                    &format!("failed to connect to daemon at {socket_path}: {e}"),
                );
                return ExitCode::from(2);
            }
        },
        None => match IpcClient::connect() {
            Ok(c) => c,
            Err(e) => {
                print_error(
                    output_mode,
                    "daemon_not_running",
                    &format!(
                        "no oxidezapd daemon listening: {e}. Start the daemon or ensure it is running."
                    ),
                );
                return ExitCode::from(2);
            }
        },
    };

    // Perform handshake
    let handshake_req = ClientRequest::Hello {
        protocol: CURRENT_PROTOCOL_VERSION,
        client_name: "oxidezap-cli".into(),
        read_only: cli.read_only,
        session_events: cli.events,
    };

    if let Err(err) = client.request(handshake_req) {
        print_error(output_mode, &err.code, &err.message);
        return ExitCode::from(3);
    }

    // Execute command
    match execute_command(&mut client, command, output_mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            print_error(output_mode, &err.code, &err.message);
            ExitCode::from(1)
        }
    }
}

fn execute_command(
    client: &mut IpcClient,
    command: Commands,
    output_mode: OutputMode,
) -> Result<(), oxidezap_wire::ApiError> {
    match command {
        Commands::Status(_) => {
            let resp = client.request(ClientRequest::GetStatus)?;
            if let DaemonResponse::Status(status) = resp {
                print_result(output_mode, &status, |s| {
                    println!("Connection: {}", s.state);
                    if let Some(name) = &s.name {
                        println!("Name:       {name}");
                    }
                    if let Some(phone) = &s.phone {
                        println!("Phone:      {phone}");
                    }
                    if let Some(jid) = &s.jid {
                        println!("JID:        {jid}");
                    }
                });
            }
            Ok(())
        }
        Commands::Auth(auth) => {
            if auth.logout {
                client.request(ClientRequest::ForgetSession)?;
                println!("Logged out successfully.");
            } else {
                let resp = client.request(ClientRequest::GetStatus)?;
                if let DaemonResponse::Status(s) = resp {
                    print_result(output_mode, &s, |status| {
                        if let Some(code) = &status.pair_code {
                            println!("Pairing Code: {code}");
                        } else if let Some(qr) = &status.qr_ascii {
                            println!("{qr}");
                        } else {
                            println!("Status: {}", status.state);
                        }
                    });
                }
            }
            Ok(())
        }
        Commands::Chats(chats) => match chats.command {
            Some(args::ChatsSubcommand::List(list)) => {
                let resp = client.request(ClientRequest::ListChats {
                    limit: list.limit,
                    offset: None,
                    query: list.query,
                    archived: list.archived,
                })?;
                if let DaemonResponse::Chats { chats, .. } = resp {
                    print_result(output_mode, &chats, |items| {
                        println!("{:<32} {:<6} NAME", "JID", "UNREAD");
                        println!("{:-<32} {:-<6} {:-<20}", "", "", "");
                        for c in items {
                            println!("{:<32} {:<6} {}", c.jid, c.unread_count, c.name);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ChatsSubcommand::Show(show)) => {
                let resp = client.request(ClientRequest::GetChat { jid: show.jid })?;
                if let DaemonResponse::Chat(c) = resp {
                    print_result(output_mode, &c, |chat| {
                        println!("JID:     {}", chat.jid);
                        println!("Name:    {}", chat.name);
                        println!("Unread:  {}", chat.unread_count);
                        println!("Pinned:  {}", chat.is_pinned);
                        println!("Muted:   {}", chat.is_muted);
                    });
                }
                Ok(())
            }
            Some(args::ChatsSubcommand::MarkRead(arg)) => {
                client.request(ClientRequest::MarkRead {
                    chat_jid: arg.jid,
                    through_message_id: None,
                })?;
                println!("Chat marked as read.");
                Ok(())
            }
            Some(args::ChatsSubcommand::MarkUnread(arg)) => {
                client.request(ClientRequest::MarkUnread { chat_jid: arg.jid })?;
                println!("Chat marked as unread.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Pin(arg)) => {
                client.request(ClientRequest::PinChat {
                    chat_jid: arg.jid,
                    pin: true,
                })?;
                println!("Chat pinned.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Unpin(arg)) => {
                client.request(ClientRequest::PinChat {
                    chat_jid: arg.jid,
                    pin: false,
                })?;
                println!("Chat unpinned.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Mute(arg)) => {
                client.request(ClientRequest::MuteChat {
                    chat_jid: arg.jid,
                    mute_duration_seconds: None,
                })?;
                println!("Chat muted.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Unmute(arg)) => {
                client.request(ClientRequest::MuteChat {
                    chat_jid: arg.jid,
                    mute_duration_seconds: Some(0),
                })?;
                println!("Chat unmuted.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Archive(arg)) => {
                client.request(ClientRequest::ArchiveChat {
                    chat_jid: arg.jid,
                    archive: true,
                })?;
                println!("Chat archived.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Unarchive(arg)) => {
                client.request(ClientRequest::ArchiveChat {
                    chat_jid: arg.jid,
                    archive: false,
                })?;
                println!("Chat unarchived.");
                Ok(())
            }
            Some(args::ChatsSubcommand::Cleanup(_)) => {
                let resp = client.request(ClientRequest::CleanupChats)?;
                if let DaemonResponse::ChatsCleaned { removed } = resp {
                    println!("Cleaned {removed} empty chats.");
                }
                Ok(())
            }
            None => {
                // Default to list
                let resp = client.request(ClientRequest::ListChats {
                    limit: 50,
                    offset: None,
                    query: None,
                    archived: false,
                })?;
                if let DaemonResponse::Chats { chats, .. } = resp {
                    print_result(output_mode, &chats, |items| {
                        println!("{:<32} {:<6} NAME", "JID", "UNREAD");
                        println!("{:-<32} {:-<6} {:-<20}", "", "", "");
                        for c in items {
                            println!("{:<32} {:<6} {}", c.jid, c.unread_count, c.name);
                        }
                    });
                }
                Ok(())
            }
        },
        Commands::Messages(msgs) => match msgs.command {
            Some(args::MessagesSubcommand::List(list)) => {
                let resp = client.request(ClientRequest::ListMessages {
                    chat_jid: list.chat,
                    limit: list.limit,
                    before: list.before,
                    after: list.after,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            let prefix = if m.from_me { "You: " } else { "" };
                            println!("[{}] {prefix}{text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Show(show)) => {
                let resp = client.request(ClientRequest::GetMessage {
                    chat_jid: show.chat,
                    message_id: show.id,
                })?;
                if let DaemonResponse::Message(m) = resp {
                    print_result(output_mode, &m, |msg| {
                        println!("ID:        {}", msg.id);
                        println!("Chat:      {}", msg.chat_jid);
                        println!("Sender:    {}", msg.sender_jid);
                        println!("Timestamp: {}", msg.timestamp_ms);
                        println!("Text:      {}", msg.text.as_deref().unwrap_or(""));
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Context(ctx)) => {
                let resp = client.request(ClientRequest::GetMessageContext {
                    chat_jid: ctx.chat,
                    message_id: ctx.id,
                    limit: ctx.limit,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            let prefix = if m.from_me { "You: " } else { "" };
                            println!("[{}] {prefix}{text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Search(s)) => {
                let resp = client.request(ClientRequest::SearchMessages {
                    query: s.query,
                    chat_jid: s.chat,
                    has_media: s.has_media,
                    limit: s.limit,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        println!("Found {} results:", items.len());
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] in {}: {text}", m.id, m.chat_jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Starred(s)) => {
                let resp = client.request(ClientRequest::ListStarredMessages { limit: s.limit })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] in {}: {text}", m.id, m.chat_jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Edit(e)) => {
                client.request(ClientRequest::EditMessage {
                    chat_jid: e.chat,
                    message_id: e.id,
                    new_text: e.text,
                })?;
                println!("Message edited.");
                Ok(())
            }
            Some(args::MessagesSubcommand::Revoke(r)) => {
                client.request(ClientRequest::RevokeMessage {
                    chat_jid: r.chat,
                    message_id: r.id,
                    for_everyone: r.for_everyone,
                })?;
                println!("Message revoked.");
                Ok(())
            }
            Some(args::MessagesSubcommand::Forward(f)) => {
                let resp = client.request(ClientRequest::ForwardMessage {
                    source_chat_jid: f.from,
                    message_id: f.id,
                    target_chat_jid: f.to,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Message forwarded with ID: {id}");
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Export(e)) => {
                let mut exported = Vec::new();
                let mut before: Option<String> = None;
                while exported.len() < e.limit {
                    let want = (e.limit - exported.len()).min(200);
                    let resp = client.request(ClientRequest::ListMessages {
                        chat_jid: e.chat.clone(),
                        limit: want,
                        before: before.clone(),
                        after: None,
                    })?;
                    let DaemonResponse::Messages { messages, .. } = resp else {
                        break;
                    };
                    if messages.is_empty() {
                        break;
                    }
                    before = messages.first().map(|m| m.id.clone());
                    let reached_start = messages.len() < want;
                    exported.extend(messages);
                    if reached_start {
                        break;
                    }
                }
                exported.reverse();
                if let Some(path) = e.output {
                    let json = serde_json::to_string_pretty(&exported).unwrap_or_default();
                    if let Err(err) = std::fs::write(&path, json) {
                        print_error(
                            output_mode,
                            "export_write_failed",
                            &format!("could not write {path}: {err}"),
                        );
                        return Ok(());
                    }
                    println!("Exported {} messages to {path}.", exported.len());
                } else {
                    print_result(output_mode, &exported, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] {text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Purge(p)) => {
                let resp = client.request(ClientRequest::PurgeMessages { chat_jid: p.chat })?;
                if let DaemonResponse::MessagesPurged { purged } = resp {
                    println!("Purged payload of {purged} revoked messages.");
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Send(send) => match send.command {
            Some(args::SendSubcommand::Text(t)) => {
                let resp = client.request(ClientRequest::SendText {
                    to: t.to,
                    message: t.message,
                    reply_to: t.reply_to,
                    mentions: t.mentions,
                    enqueue_only: t.enqueue,
                })?;
                if let DaemonResponse::MessageSent { id, enqueued, .. } = resp {
                    if enqueued {
                        println!("Message enqueued with ID: {id}");
                    } else {
                        println!("Message sent with ID: {id}");
                    }
                }
                Ok(())
            }
            Some(args::SendSubcommand::File(f)) => {
                let resp = client.request(ClientRequest::SendMedia {
                    to: f.to,
                    file_path: f.file,
                    caption: f.caption,
                    mime_type: None,
                    as_document: f.as_document,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("File sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::Voice(v)) => {
                let resp = client.request(ClientRequest::SendAudio {
                    to: v.to,
                    file_path: v.file,
                    ptt: true,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Voice note sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::React(r)) => {
                let resp = client.request(ClientRequest::SendReaction {
                    chat_jid: r.chat,
                    message_id: r.id,
                    emoji: r.emoji,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Reaction sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::Poll(p)) => {
                let resp = client.request(ClientRequest::SendPoll {
                    to: p.to,
                    question: p.question,
                    options: p.options,
                    selectable_count: p.selectable,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Poll sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::Location(loc)) => {
                let resp = client.request(ClientRequest::SendLocation {
                    to: loc.to,
                    latitude: loc.lat,
                    longitude: loc.lng,
                    name: loc.name,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Location sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::Status(st)) => {
                let resp = client.request(ClientRequest::SendStatus { text: st.text })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Status broadcast sent with ID: {id}");
                }
                Ok(())
            }
            Some(args::SendSubcommand::Sticker(s)) => {
                let resp = client.request(ClientRequest::SendSticker {
                    to: s.to,
                    file_path: s.file,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    println!("Sticker sent with ID: {id}");
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Contacts(contacts) => match contacts.command {
            Some(args::ContactsSubcommand::Search(s)) => {
                let resp = client.request(ClientRequest::ListContacts {
                    query: s.query,
                    limit: s.limit,
                })?;
                if let DaemonResponse::Contacts { contacts } = resp {
                    print_result(output_mode, &contacts, |items| {
                        for c in items {
                            let name = c.name.as_deref().or(c.push_name.as_deref()).unwrap_or("");
                            println!("{:<32} {name}", c.jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Check(chk)) => {
                let resp = client.request(ClientRequest::CheckContact { phone: chk.phone })?;
                if let DaemonResponse::ContactCheck {
                    phone,
                    is_registered,
                    jid,
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "phone": phone, "registered": is_registered, "jid": jid }),
                        |_| {
                            println!(
                                "Phone {phone}: registered={is_registered} (JID: {})",
                                jid.as_deref().unwrap_or("none")
                            );
                        },
                    );
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetContact { jid: s.jid })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, |contact| {
                        print_contact(contact);
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Refresh(r)) => {
                let resp = client.request(ClientRequest::RefreshContacts { jid: r.jid })?;
                if let DaemonResponse::Contacts { contacts } = resp {
                    print_result(output_mode, &contacts, |items| {
                        for c in items {
                            print_contact(c);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Alias(a)) => {
                let resp = client.request(ClientRequest::SetContactAlias {
                    jid: a.jid,
                    alias: a.alias,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, |contact| {
                        print_contact(contact);
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Tag(t)) => {
                let resp = client.request(ClientRequest::TagContact {
                    jid: t.jid,
                    tag: t.tag,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, |contact| {
                        print_contact(contact);
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Untag(u)) => {
                let resp = client.request(ClientRequest::UntagContact {
                    jid: u.jid,
                    tag: u.tag,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, |contact| {
                        print_contact(contact);
                    });
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Groups(groups) => match groups.command {
            Some(args::GroupsSubcommand::List(l)) => {
                let resp = client.request(ClientRequest::ListGroups { refresh: l.refresh })?;
                if let DaemonResponse::Groups { groups } = resp {
                    print_result(output_mode, &groups, |items| {
                        println!("{:<32} {:<8} SUBJECT", "JID", "MEMBERS");
                        for g in items {
                            println!("{:<32} {:<8} {}", g.jid, g.participant_count, g.subject);
                        }
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Info(i)) => {
                let resp = client.request(ClientRequest::GetGroupInfo { group_jid: i.jid })?;
                if let DaemonResponse::Group(g) = resp {
                    print_result(output_mode, &g, |grp| {
                        println!("Subject:      {}", grp.subject);
                        println!("JID:          {}", grp.jid);
                        println!(
                            "Owner:        {}",
                            grp.owner_jid.as_deref().unwrap_or("none")
                        );
                        println!("Participants: {}", grp.participant_count);
                        println!("Description:  {}", grp.description.as_deref().unwrap_or(""));
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Create(c)) => {
                let resp = client.request(ClientRequest::CreateGroup {
                    subject: c.subject,
                    participants: c.participants,
                })?;
                if let DaemonResponse::GroupCreated { jid } = resp {
                    println!("Group created: {jid}");
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Rename(r)) => {
                client.request(ClientRequest::SetGroupTopic {
                    group_jid: r.jid,
                    topic: r.title,
                })?;
                println!("Group renamed.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Description(d)) => {
                client.request(ClientRequest::SetGroupDescription {
                    group_jid: d.jid,
                    description: d.description,
                })?;
                println!("Group description updated.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Add(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Add,
                })?;
                println!("Participant added.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Remove(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Remove,
                })?;
                println!("Participant removed.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Promote(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Promote,
                })?;
                println!("Participant promoted to admin.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Demote(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Demote,
                })?;
                println!("Admin demoted to regular participant.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Leave(l)) => {
                client.request(ClientRequest::LeaveGroup { group_jid: l.jid })?;
                println!("Left group successfully.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Invite(i)) => {
                let resp = client.request(ClientRequest::GetGroupInviteLink {
                    group_jid: i.jid,
                    reset: i.reset,
                })?;
                if let DaemonResponse::GroupInviteLink { link } = resp {
                    println!("{link}");
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Join(j)) => {
                let resp = client.request(ClientRequest::JoinGroup {
                    invite_code: j.code,
                })?;
                if let DaemonResponse::GroupJoined {
                    jid,
                    pending_approval,
                } = resp
                {
                    if pending_approval {
                        println!("Join requested for {jid}, awaiting admin approval.");
                    } else {
                        println!("Joined group: {jid}");
                    }
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Permissions(p)) => {
                client.request(ClientRequest::SetGroupPermissions {
                    group_jid: p.jid,
                    announce_only: p.announce_only,
                    locked: p.locked,
                })?;
                println!("Group permissions updated.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Requests(r)) => {
                let resp =
                    client.request(ClientRequest::ListGroupJoinRequests { group_jid: r.jid })?;
                if let DaemonResponse::GroupJoinRequests { requests } = resp {
                    print_result(output_mode, &requests, |items| {
                        for req in items {
                            println!("{}", req.jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Approve(a)) => {
                client.request(ClientRequest::ManageGroupJoinRequest {
                    group_jid: a.group,
                    participant_jid: a.participant,
                    approve: true,
                })?;
                println!("Membership request approved.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Reject(r)) => {
                client.request(ClientRequest::ManageGroupJoinRequest {
                    group_jid: r.group,
                    participant_jid: r.participant,
                    approve: false,
                })?;
                println!("Membership request rejected.");
                Ok(())
            }
            Some(args::GroupsSubcommand::Prune(_)) => {
                let resp = client.request(ClientRequest::CleanupChats)?;
                if let DaemonResponse::ChatsCleaned { removed } = resp {
                    println!("Pruned {removed} empty chats.");
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Poll(poll) => match poll.command {
            Some(args::PollSubcommand::List(l)) => {
                let resp = client.request(ClientRequest::ListPolls {
                    chat_jid: l.chat,
                    limit: l.limit,
                })?;
                if let DaemonResponse::Polls { polls } = resp {
                    print_result(output_mode, &polls, |items| {
                        for p in items {
                            println!("[{}] in {}: {}", p.id, p.chat_jid, p.question);
                        }
                    });
                }
                Ok(())
            }
            Some(args::PollSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetPoll {
                    chat_jid: s.chat,
                    poll_id: s.poll,
                })?;
                if let DaemonResponse::Poll(p) = resp {
                    print_result(output_mode, &p, |poll| {
                        println!("Question: {}", poll.question);
                        for opt in &poll.options {
                            println!("  [{}] {} ({} votes)", opt.index, opt.name, opt.vote_count);
                        }
                    });
                }
                Ok(())
            }
            Some(args::PollSubcommand::Vote(v)) => {
                client.request(ClientRequest::VotePoll {
                    chat_jid: v.chat,
                    poll_id: v.poll,
                    selected_option_indices: v.options,
                })?;
                println!("Vote registered.");
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Profile(profile) => match profile.command {
            Some(args::ProfileSubcommand::Get(g)) => {
                let resp = client.request(ClientRequest::GetProfile { jid: g.jid })?;
                if let DaemonResponse::Profile(p) = resp {
                    print_result(output_mode, &p, |prof| {
                        println!("Name:  {}", prof.name.as_deref().unwrap_or("none"));
                        println!("About: {}", prof.about.as_deref().unwrap_or("none"));
                        println!("Photo: {}", prof.picture_url.as_deref().unwrap_or("none"));
                    });
                }
                Ok(())
            }
            Some(args::ProfileSubcommand::SetAbout(a)) => {
                client.request(ClientRequest::SetProfileAbout { about: a.text })?;
                println!("Profile about updated.");
                Ok(())
            }
            Some(args::ProfileSubcommand::SetName(n)) => {
                client.request(ClientRequest::SetProfileName { name: n.name })?;
                println!("Profile name updated.");
                Ok(())
            }
            Some(args::ProfileSubcommand::SetPicture(p)) => {
                client.request(ClientRequest::SetProfilePicture { file_path: p.file })?;
                println!("Profile picture updated.");
                Ok(())
            }
            Some(args::ProfileSubcommand::RemovePicture(_)) => {
                client.request(ClientRequest::RemoveProfilePicture)?;
                println!("Profile picture removed.");
                Ok(())
            }
            Some(args::ProfileSubcommand::Business(b)) => {
                // No JID means this account: resolve it from the status.
                let jid = match b.jid {
                    Some(jid) => jid,
                    None => match client.request(ClientRequest::GetStatus)? {
                        DaemonResponse::Status(status) => status.jid.unwrap_or_default(),
                        _ => String::new(),
                    },
                };
                if jid.is_empty() {
                    print_error(output_mode, "not_connected", "no account JID available");
                    return Ok(());
                }
                let resp = client.request(ClientRequest::GetBusinessProfile { jid })?;
                if let DaemonResponse::Profile(p) = resp {
                    print_result(output_mode, &p, |prof| {
                        println!("Name:  {}", prof.name.as_deref().unwrap_or("none"));
                        println!("About: {}", prof.about.as_deref().unwrap_or("none"));
                        println!("Photo: {}", prof.picture_url.as_deref().unwrap_or("none"));
                    });
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Presence(presence) => match presence.command {
            Some(args::PresenceSubcommand::Typing(t)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: t.chat,
                    state: oxidezap_wire::dto::PresenceState::Composing,
                })?;
                println!("Sent composing indicator.");
                Ok(())
            }
            Some(args::PresenceSubcommand::Paused(p)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: p.chat,
                    state: oxidezap_wire::dto::PresenceState::Paused,
                })?;
                println!("Sent paused indicator.");
                Ok(())
            }
            Some(args::PresenceSubcommand::Recording(r)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: r.chat,
                    state: oxidezap_wire::dto::PresenceState::Recording,
                })?;
                println!("Sent recording indicator.");
                Ok(())
            }
            Some(args::PresenceSubcommand::Online(_)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: None,
                    state: oxidezap_wire::dto::PresenceState::Available,
                })?;
                println!("Appearing online.");
                Ok(())
            }
            Some(args::PresenceSubcommand::Offline(_)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: None,
                    state: oxidezap_wire::dto::PresenceState::Unavailable,
                })?;
                println!("Appearing offline.");
                Ok(())
            }
            None => Ok(()),
        },
        Commands::History(history) => match history.command {
            Some(args::HistorySubcommand::Coverage(c)) => {
                let resp = client.request(ClientRequest::HistoryCoverage {
                    chat_jid: c.chat.clone(),
                })?;
                if let DaemonResponse::HistoryCoverage {
                    stored_count,
                    oldest_ts,
                    newest_ts,
                    ..
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({
                            "stored_count": stored_count,
                            "oldest_ts": oldest_ts,
                            "newest_ts": newest_ts,
                        }),
                        |_| {
                            println!("Stored messages: {stored_count}");
                            println!(
                                "Oldest: {}",
                                oldest_ts
                                    .map(|t| t.to_string())
                                    .as_deref()
                                    .unwrap_or("none")
                            );
                            println!(
                                "Newest: {}",
                                newest_ts
                                    .map(|t| t.to_string())
                                    .as_deref()
                                    .unwrap_or("none")
                            );
                        },
                    );
                }
                Ok(())
            }
            Some(args::HistorySubcommand::Backfill(b)) => {
                let resp = client.request(ClientRequest::HistoryBackfill {
                    chat_jid: b.chat,
                    count: b.count,
                })?;
                if let DaemonResponse::HistoryCoverage { stored_count, .. } = resp {
                    println!("Backfilled history ({stored_count} messages stored).");
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Channels(channels) => match channels.command {
            Some(args::ChannelsSubcommand::List(_)) => {
                let resp = client.request(ClientRequest::ListChannels)?;
                if let DaemonResponse::Channels { channels } = resp {
                    print_result(output_mode, &channels, |items| {
                        for c in items {
                            println!("{:<32} {} ({})", c.jid, c.name, c.subscriber_count);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetChannelInfo { channel_jid: s.jid })?;
                if let DaemonResponse::Channel(c) = resp {
                    print_result(output_mode, &c, |channel| {
                        println!("Name:        {}", channel.name);
                        println!("JID:         {}", channel.jid);
                        println!("Subscribers: {}", channel.subscriber_count);
                        println!(
                            "Description: {}",
                            channel.description.as_deref().unwrap_or("")
                        );
                    });
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Join(j)) => {
                let resp = client.request(ClientRequest::JoinChannel { channel_jid: j.jid })?;
                if let DaemonResponse::Channel(c) = resp {
                    println!("Following channel: {}", c.name);
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Leave(l)) => {
                client.request(ClientRequest::LeaveChannel { channel_jid: l.jid })?;
                println!("Unfollowed channel.");
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Accounts(accounts) => match accounts.command {
            Some(args::AccountsSubcommand::List(_)) => {
                let resp = client.request(ClientRequest::ListAccounts)?;
                if let DaemonResponse::Accounts { accounts } = resp {
                    print_result(output_mode, &accounts, |items| {
                        println!("{:<16} {:<8} SOCKET", "ID", "ACTIVE");
                        for a in items {
                            println!(
                                "{:<16} {:<8} {}",
                                a.id,
                                if a.active { "yes" } else { "no" },
                                a.socket_path
                            );
                        }
                    });
                }
                Ok(())
            }
            // Handled locally before connecting; unreachable here.
            Some(args::AccountsSubcommand::Use(_)) => Ok(()),
            None => Ok(()),
        },
        Commands::Calls(c) => {
            let resp = client.request(ClientRequest::ListCalls { limit: c.limit })?;
            if let DaemonResponse::Calls { calls } = resp {
                print_result(output_mode, &calls, |items| {
                    println!("{:<24} {:<10} DURATION", "CALLER", "OUTCOME");
                    for call in items {
                        println!(
                            "{:<24} {:<10} {}s",
                            call.caller_jid,
                            call.outcome,
                            call.duration_seconds.unwrap_or(0)
                        );
                    }
                });
            }
            Ok(())
        }
        Commands::Media(media) => match media.command {
            Some(args::MediaSubcommand::Download(d)) => {
                let resp = client.request(ClientRequest::DownloadMedia {
                    chat_jid: d.chat,
                    message_id: d.id,
                    destination: d.output,
                })?;
                if let DaemonResponse::MediaDownloaded {
                    local_path,
                    size_bytes,
                    ..
                } = resp
                {
                    println!("Media downloaded to {local_path} ({size_bytes} bytes).");
                }
                Ok(())
            }
            Some(args::MediaSubcommand::Retry(r)) => {
                client.request(ClientRequest::RetryMedia {
                    chat_jid: r.chat,
                    message_id: r.id,
                })?;
                println!("Requested media re-upload from primary device.");
                Ok(())
            }
            Some(args::MediaSubcommand::Backfill(b)) => {
                let resp = client.request(ClientRequest::BackfillMedia {
                    chat_jid: b.chat,
                    limit: b.limit,
                })?;
                if let DaemonResponse::MediaBackfilled {
                    requested,
                    downloaded,
                } = resp
                {
                    println!("Backfilled {downloaded} of {requested} media files.");
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Store(store) => match store.command {
            Some(args::StoreSubcommand::Stats(_)) => {
                let resp = client.request(ClientRequest::GetStorageUsage)?;
                if let DaemonResponse::Storage(s) = resp {
                    print_result(output_mode, &s, |storage| {
                        println!(
                            "Database:    {:.2} MB",
                            storage.database_bytes as f64 / 1_048_576.0
                        );
                        println!(
                            "Media cache: {:.2} MB ({} files)",
                            storage.media_bytes as f64 / 1_048_576.0,
                            storage.media_files
                        );
                    });
                }
                Ok(())
            }
            Some(args::StoreSubcommand::Cleanup(_)) => {
                client.request(ClientRequest::ClearMediaCache)?;
                println!("Media cache cleaned.");
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Doctor(_) => {
            let resp = client.request(ClientRequest::DoctorCheck)?;
            if let DaemonResponse::Doctor(d) = resp {
                print_result(output_mode, &d, |doctor| {
                    println!(
                        "Daemon:      {}",
                        if doctor.daemon_running {
                            "running"
                        } else {
                            "stopped"
                        }
                    );
                    println!("Socket:      {}", doctor.socket_path);
                    println!("Connection:  {}", doctor.connection_state);
                    println!(
                        "Database:    {}",
                        if doctor.database_ok {
                            "healthy"
                        } else {
                            "error"
                        }
                    );
                    println!(
                        "DB size:     {:.2} MB",
                        doctor.database_bytes as f64 / 1_048_576.0
                    );
                    println!("Media cache: {}", doctor.media_cache_dir);
                });
            }
            Ok(())
        }
        Commands::Sync(sync) => {
            if sync.follow {
                println!("Following events stream (Ctrl+C to stop)...");
                while let Ok(Some(event)) = client.next_event() {
                    print_event(&event);
                }
            }
            Ok(())
        }
        Commands::Completion(_) | Commands::Mcp(_) => Ok(()),
    }
}

/// One contact as a human line: name, address, and local labels.
fn print_contact(contact: &oxidezap_wire::dto::ContactDto) {
    let name = contact
        .alias
        .as_deref()
        .or(contact.name.as_deref())
        .or(contact.push_name.as_deref())
        .unwrap_or("");
    let tags = if contact.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", contact.tags.join(", "))
    };
    println!("{:<32} {name}{tags}", contact.jid);
}
