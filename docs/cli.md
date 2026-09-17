# oxidezap-cli

Scriptable WhatsApp CLI. One session per user lives in the daemon; the CLI
holds none — every command is a line-protocol request over the daemon
socket. When no daemon is listening the CLI starts one beside itself, the
same way the window does, so a first `oxidezap-cli status` on a fresh
machine works without a separate launch.

The binary ships in the same archive as the window and the daemon, in the
`oxidezap-cli` file beside `oxidezap` and `oxidezapd`, so a release download
is enough to use it. To build it from source:

```bash
cargo build --release -p oxidezap-cli
./target/release/oxidezap-cli status
```

`oxidezap-cli --help` is the command authority; what follows is how to
think about it, not a second copy of it.

## Output

Human tables by default. `--json` prints one envelope to stdout:

```json
{ "ok": true, "data": { } }
```

and errors to stderr as
`{"ok": false, "error": {"code": "...", "message": "..."}}` with a nonzero
exit. Every command, read or write, returns a DTO in `data`; no command
prints a human sentence in `--json`, and none exits zero after a failure.

Three commands write raw text rather than a DTO: `completion` (a shell
script), `mcp` (a usage spec) and `accounts use` (an `eval` line). They
refuse `--json`/`--events` with `output_mode_unsupported` and a nonzero
exit instead of putting non-JSON on stdout.

`--events` subscribes the connection to lifecycle events. `sync --follow`
prints one `DaemonEvent` per line as NDJSON and nothing else, so a follower
can parse every line it reads. Exit codes: `0` ok, `1` the command or the
API call failed, `2` invalid input or no daemon (and it could not be
started), `3` handshake rejected. Code `1` is the general failure: a daemon
refusal, an unsupported output mode, a local read-only violation, and
anything else the command could not carry out.

## Safety

`--read-only` (or `OXIDEZAP_READONLY=1`) refuses every mutating action
client-side *and* daemon-side: the daemon answers mutations with
`permission_denied` before touching the session. What counts as a mutation
is declared per request in `oxidezap-wire`'s `ClientRequest::access`, which
is an exhaustive `match` — a new request does not compile until it says
whether it reads or writes, so a variant cannot be born permissive by
accident. Mutations need a connected account; reads work offline against
the local store.

`accounts add` and `accounts remove` run locally, before any handshake, so
the daemon never sees them; `--read-only` refuses both in the client with
`read_only_violation`. Without that the flag would still permit the one
command that deletes a profile's store.

## Accounts

One account is one daemon over one store: separate socket, lock, database
and media cache. The daemon takes `--account <id>` (or `OXIDEZAP_ACCOUNT`);
the CLI selects with the same flag or variable, and `--socket` overrides
both. `--account` wins over the environment.

An id is validated, never sanitized: `[A-Za-z0-9][A-Za-z0-9_-]*`, and
anything else is refused with `invalid_account_id` (exit 2). Sanitizing
would map `wo/rk` onto `work` — two profiles converging on one store — and
the same value is echoed by `accounts use` into a shell, so it must not
carry a quote, a space or a `$` either.

`accounts list` shows the profiles on this machine and whether each daemon
is up; `eval $(oxidezap-cli accounts use <id>)` selects one for the shell,
and `accounts use default` prints `unset OXIDEZAP_ACCOUNT`. `accounts add
<id>` validates the id and starts that profile's daemon; `accounts remove
<id>` wipes the profile's store through its own daemon, deletes its media
cache and stops the process. Without an id everything is the default
profile on the historic paths, unchanged.

## Auth

`auth` prints the QR or the pairing code from the connection snapshot.
`auth --phone <number>` asks the primary device for a phone-number pairing
code and prints it with its deadline. `auth --logout` wipes local state so
the account can pair again.

## Pagination

`messages list --chat <jid>` walks back from the newest message. Pass
`--before <cursor>` to continue that walk: the cursor is the opaque
`next_cursor` the previous answer returned, not a message id. A message id
is a different string and does not parse back into the position the page
was read from, so replacing the cursor with one skips or repeats rows.
`--after <cursor>` walks *forward* from a position you already hold, oldest
first, which is the direction a sync loop needs, and takes the same opaque
cursor. The two are exclusive; a request carrying both is answered as the
forward page.

## Events

`sync` prints the current status and exits. `sync --follow` asks the daemon
for session events in its handshake and follows the stream as NDJSON.
Receipts, reactions and call stages have no event spelling — poll or re-list
for those. The follower skips lines it cannot parse; EOF ends it cleanly and
a transport error is reported with a nonzero exit.

## Examples

```bash
# Read-only agent pass: status, unread chats, recent messages as JSON.
oxidezap-cli --read-only --json status
oxidezap-cli --read-only --json chats list --limit 20
oxidezap-cli --read-only --json messages list --chat 5511999999999@s.whatsapp.net

# Walk forward from a message you hold.
oxidezap-cli messages list --chat 5511999999999@s.whatsapp.net --after 3EB0ABCD

# Send and poll.
oxidezap-cli send text --to 5511999999999@s.whatsapp.net "oi" --reply-to 3EB0ABCD
oxidezap-cli send text --to 5511999999999@s.whatsapp.net "oi" --enqueue
oxidezap-cli poll list --chat 5511999999999@s.whatsapp.net
oxidezap-cli poll vote --chat 5511999999999@s.whatsapp.net --poll 3EB0ABCD 0

# Pair by phone number instead of QR.
oxidezap-cli auth --phone 5511999999999

# Answer a bot menu.
oxidezap-cli send select --to 5511999999999@s.whatsapp.net --row-id row-1

# Groups, channels, history.
oxidezap-cli groups create "almoço" --participant 5511999999999
oxidezap-cli groups invite --jid 1234567890@g.us
oxidezap-cli channels join --jid 123@newsletter
oxidezap-cli history coverage
oxidezap-cli history backfill 5511999999999@s.whatsapp.net

# Accounts.
oxidezap-cli accounts add work
eval $(oxidezap-cli accounts use work)
oxidezap-cli accounts remove work

# Completions and agent interfaces.
oxidezap-cli completion --shell bash >> ~/.bashrc
oxidezap-cli mcp   # usage-spec KDL for MCP hosts
```

## Limits worth knowing

- Poll listing shows the creation (question, options). Live tallies are not
  decrypted: a vote arrives encrypted to the creation's secret, and
  counting them is a separate pass the store does not keep.
- `history backfill` asks the primary phone for older messages through
  PDO, then re-reads the local store. A phone that refuses the request is
  not fatal; what it warms is what already exists here.
- `calls list` covers the daemon's lifetime; calls are live state, not rows
  in the store.
- There is no `oxidezapd-headless` without the VoIP stack yet. Calls,
  cameras and the codec rest on the library's `voip` feature, which cargo
  selects per target rather than per dependency, so a crate feature cannot
  turn it off for the daemon while the page build keeps `voip-mlow`. The
  `--headless` flag means "no system tray", not "no video plane".
