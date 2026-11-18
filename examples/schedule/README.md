# Scheduled messages

Build the plugin from the repository root:

```sh
cargo xtask plugin build examples/schedule
```

Install the resulting `schedule.wasm` in Settings → Plugins and approve
sending messages. In a direct chat or group, open **Chat tools** by the
composer. Enter the message and either minutes from now or an RFC 3339
time with an explicit offset, such as `2026-11-21T14:30:00-03:00`.
Press **Schedule**. The plugin's Settings panel lists pending messages and
lets you cancel them.

The queue is stored per account. Keep the daemon running on desktop or
the app's host tab open on web for on-time attempts. The plugin checks due
messages when it starts and every five seconds thereafter. A disconnected
account keeps its jobs until it reconnects.

The plugin records an attempt before handing the message to the account.
An accepted command is handed off once. Acceptance is not a WhatsApp
delivery receipt. A crash in the handoff leaves an unconfirmed entry in
Settings for manual inspection. The plugin does not replay it automatically.
