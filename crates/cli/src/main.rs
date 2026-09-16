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
                client.request(ClientRequest::ForwardMessage {
                    source_chat_jid: f.from,
                    message_id: f.id,
                    target_chat_jid: f.to,
                })?;
                println!("Message forwarded.");
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
                client.request(ClientRequest::SendMedia {
                    to: f.to,
                    file_path: f.file,
                    caption: f.caption,
                    mime_type: None,
                    as_document: f.as_document,
                })?;
                println!("File sent successfully.");
                Ok(())
            }
            Some(args::SendSubcommand::Voice(v)) => {
                client.request(ClientRequest::SendAudio {
                    to: v.to,
                    file_path: v.file,
                    ptt: true,
                })?;
                println!("Voice note sent.");
                Ok(())
            }
            Some(args::SendSubcommand::React(r)) => {
                client.request(ClientRequest::SendReaction {
                    chat_jid: r.chat,
                    message_id: r.id,
                    emoji: r.emoji,
                })?;
                println!("Reaction updated.");
                Ok(())
            }
            Some(args::SendSubcommand::Poll(p)) => {
                client.request(ClientRequest::SendPoll {
                    to: p.to,
                    question: p.question,
                    options: p.options,
                    selectable_count: p.selectable,
                })?;
                println!("Poll sent successfully.");
                Ok(())
            }
            Some(args::SendSubcommand::Location(loc)) => {
                client.request(ClientRequest::SendLocation {
                    to: loc.to,
                    latitude: loc.lat,
                    longitude: loc.lng,
                    name: loc.name,
                })?;
                println!("Location sent.");
                Ok(())
            }
            Some(args::SendSubcommand::Status(st)) => {
                client.request(ClientRequest::SendStatus { text: st.text })?;
                println!("Status broadcast sent.");
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
                client.request(ClientRequest::CreateGroup {
                    subject: c.subject,
                    participants: c.participants,
                })?;
                println!("Group created successfully.");
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
            None => Ok(()),
        },
        Commands::Poll(poll) => match poll.command {
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
