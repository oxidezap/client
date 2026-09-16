# oxidezap-cli

Scriptable WhatsApp CLI. One session per user lives in the daemon; the CLI
holds none — every command is a line-protocol request over the daemon
socket, and the window looks for the daemon beside itself, so run the
daemon first (`oxidezapd`, or the desktop app, which embeds one).

```bash
cargo build --release -p oxidezap-cli
./target/release/oxidezap-cli status
```

`oxidezap-cli --help` is the command authority; what follows is how to
think about it, not a second copy of it.

## Output

Human tables by default. `--json` prints pretty JSON to stdout (errors go
to stderr as `{"error": {"code", "message"}}` with a nonzero exit).
`--events` subscribes the connection to lifecycle events for commands that
stream. Exit codes: `0` ok, `1` daemon refused, `2` no daemon listening,
`3` handshake rejected.

## Safety

`--read-only` (or `OXIDEZAP_READONLY=1`) refuses every mutating action
client-side *and* daemon-side: the daemon answers mutations with
`permission_denied` before touching the session. Agents that only read
should always pass it. Mutations need a connected account; reads work
offline against the local store.

## Accounts

One account is one daemon over one store: separate socket, lock, database
and media cache. The daemon takes `--account <id>` (or `OXIDEZAP_ACCOUNT`);
the CLI selects with the same flag or variable, and `--socket` overrides
both. `accounts list` shows the profiles on this machine and whether each
daemon is up; `eval $(oxidezap-cli accounts use <id>)` selects one for the
shell. Without an id everything is the default profile on the historic
paths, unchanged.

## Events

`sync` prints the current status and exits. `sync --follow` follows the
event stream as NDJSON: message arrivals, chat updates, presence changes
and connection changes. Receipts, reactions and call stages have no event
spelling — poll or re-list for those. The follower skips lines it cannot
parse; only EOF ends it.

## Examples

```bash
# Read-only agent pass: status, unread chats, recent messages as JSON.
oxidezap-cli --read-only --json status
oxidezap-cli --read-only --json chats list --limit 20
oxidezap-cli --read-only --json messages list --chat 5511999999999@s.whatsapp.net

# Send and poll.
oxidezap-cli send text --to 5511999999999@s.whatsapp.net "oi" --reply-to 3EB0ABCD
oxidezap-cli poll list --chat 5511999999999@s.whatsapp.net
oxidezap-cli poll vote --chat 5511999999999@s.whatsapp.net --poll 3EB0ABCD 0

# Groups, channels, history.
oxidezap-cli groups create "almoço" --participant 5511999999999
oxidezap-cli groups invite --jid 1234567890@g.us
oxidezap-cli channels join --jid 123@newsletter
oxidezap-cli history coverage
oxidezap-cli media backfill --limit 20

# Completions and agent interfaces.
oxidezap-cli completion --shell bash >> ~/.bashrc
oxidezap-cli mcp   # usage-spec KDL for MCP hosts
```

## Limits worth knowing

- `send select` (interactive list answers) has no library API and is not
  offered; every other send kind is.
- Poll listing shows the creation (question, options); live tallies are not
  decrypted.
- History backfill re-reads the local store; the library offers no
  on-demand phone fetch.
- `calls list` covers the daemon's lifetime; calls are live state, not rows.
- `accounts add/remove` need daemon supervision and are not offered; run
  one `oxidezapd --account <id>` per profile instead.
