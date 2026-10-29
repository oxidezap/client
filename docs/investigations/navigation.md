# Sidebar destinations

Chats excludes newsletters and community containers. Community child groups remain
conversations in Chats and are also reachable through their parent in Communities.
Unknown group relationships remain in Chats until authoritative metadata arrives.
Channels uses newsletter JIDs; Communities uses server-reported hierarchy, never names.

Each conversation destination remembers its last selection for this window. Switching
resets the sidebar search and scroll position, keeps drafts through the existing chat
selection path, and returns a narrow window to the list. The Chats filter is retained
but does not filter other destinations. A selection hidden by a destination cannot
become a visible/read conversation. Restoring a selection does not commit a read;
the rendered conversation resumes it when visible. Status keeps its existing media lifecycle.

The rail badge is a sibling of its button. The button component clips its internal
content to the icon box, which truncates `99+` on desktop even when the badge
height and text bounds are correct. Count badges use content height plus padding.

## Official Web evidence

Inspected 2026-10-29, whatspec `eb16f595d455d8b0b3f7966a59444f4f9894c127`,
schema `4.4.0`, WhatsApp `2.3000.1047483476`. All 579 JS bundle hashes and sizes
match `generated/bundles.lock.json`, set
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The archive SHA-256 is `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.

AST-extracted complete modules in `5gZS2NLYc2o.js` establish separate navbar
items and last-active selections: `WAWebChatsNavBarItem.react`,
`WAWebNewslettersNavBarItem.react`, `WAWebCommunitiesNavBarItem.react`, and
`WAWebOpenLastActiveCommunityAction`. The last accepts community containers,
regular subgroups, announcements and general groups.
`WAWebNavBarCometRouting` maps Chats and Channels to separate routes while the
community item opens a drawer on Chats. These are snapshot observations, not a
claim about every account's current flags. This client retains its existing
in-window navigation and does not introduce browser URL/history routing.
