# Chrome Web Store — Store Listing Draft

---

## Extension name

`Golden Protocol`

## Version

`0.1.0`

## Category

`Productivity`

## Language

Primary: English

---

## Short description (≤132 chars)

```
Browse the web as a space. Your curiosity shapes your position. See who's near. No accounts, no feeds — just presence.
```

(118 chars)

---

## Detailed description

```
Golden Protocol turns your browser into a presence in a shared semantic space.

As you browse, your interests quietly accumulate into a vector. The server finds other entities — articles, topics, people — semantically near you right now. Open the popup to see what's close, what's on the horizon, and how many others are wandering nearby.

No login. No timeline. No algorithm optimizing for engagement.

──────────────────────────────
HOW IT WORKS
──────────────────────────────
• Every page you visit updates your interest vector (computed locally)
• The extension sends a compressed semantic signal to the field server
• The server computes distance based on meaning, not clicks or follows
• You appear in the field. Others appear near you.

──────────────────────────────
THE BADGE
──────────────────────────────
The gold number on the extension icon shows how many presences are in the field right now — without opening anything. Just a number. It means something is happening.

──────────────────────────────
PASSIVE ABSORPTION
──────────────────────────────
Simply browsing updates your position in the field. You drift toward what interests you without doing anything deliberately. Your position is the shape of your curiosity.

──────────────────────────────
THE FIELD
──────────────────────────────
Open the popup to see:
• near — entities and people close to your current interests
• horizon — presences detected but not yet resolved
• drift — directions the space is flowing toward

Entities are articles, topics, RSS streams, and events added by anyone running a Golden Protocol node. The space grows as people use it.

──────────────────────────────
TECHNICAL
──────────────────────────────
• Local embedding: BAAI/bge-small-en-v1.5 (384-dim, runs on server)
• Distance: weighted sum of semantic, relational, activity, temporal, and attention components
• No raw text or URLs are ever transmitted — only the semantic vector
• Open source (MIT): github.com/gold3112/golden-protocol
• Live space: space.gold3112.online

This is an experiment in what the internet feels like as a space rather than a collection of pages.
```

---

## Privacy policy URL

```
https://github.com/gold3112/golden-protocol/blob/main/docs/privacy-policy.md
```

---

## Screenshots (1280×800 or 640×400)

Recommended shots to capture:

1. **Canvas visualization** — `space.gold3112.online` with entities drifting, gold cross at center
2. **Popup open** — near/horizon entity list, gold badge visible on toolbar icon
3. **CLI format** — terminal showing `curl space.gold3112.online/field?format=text` output
4. **Badge close-up** — browser toolbar with gold number badge (presence count)

---

## Single purpose statement (for permission justification)

```
The extension reads the current page's text to compute a semantic interest vector,
which is used to find semantically related entities and other users in a shared
field server. No raw text or browsing history is transmitted or stored externally.
```

---

## Notes for submission

- `activeTab` + `tabs`: needed for page text extraction and SPA navigation detection
- `storage`: stores only a random UUID in chrome.storage.local
- `host_permissions`: single origin (space.gold3112.online) — no broad host access
- The extension does not inject UI into web pages (content.js only reads text and sends a message to background.js)
