# Privacy Policy — Golden Protocol Extension

*Last updated: 2026-03-17*

## Overview

Golden Protocol is a browser extension that gives you a presence in a shared semantic space. This policy explains what data the extension handles, how it is used, and what is never collected.

---

## What the extension collects

### On your device (local only)

| Data | Where stored | Purpose |
|------|-------------|---------|
| A random UUID (`userId`) | `chrome.storage.local` | Identifies your presence in the field across sessions |
| Current page text (title + body excerpt, ≤600 chars) | `chrome.storage.session` | Temporary; used only to compute your interest vector for the current session |

### Sent to the server (`space.gold3112.online`)

| Data | What is sent | What is NOT sent |
|------|-------------|-----------------|
| Interest signal | A 384-dimensional floating-point vector (the semantic embedding of your page text) | The raw page text, URL, or browsing history |
| User ID | Your random UUID | Any personally identifiable information |

The server receives a compressed representation of what you are reading — not the actual content or address. The vector cannot be reversed into readable text.

---

## What the extension does NOT collect

- No browsing history is stored or transmitted
- No personally identifiable information (name, email, IP beyond standard HTTP) is collected
- No data is sold or shared with third parties
- No analytics, crash reporting, or tracking SDKs are included

---

## Server-side data

The server (`space.gold3112.online`) stores:

- Your UUID and interest vector (updated on each page visit)
- A short encounter history (last 200 visits, stored as position labels and timestamps — not URLs)
- Any messages you voluntarily leave in the field (stored for 2 hours, then automatically deleted)

This data is not linked to any real-world identity. Deleting the extension removes your local UUID; your server-side data expires naturally within 30 days of inactivity.

---

## Permissions used

| Permission | Why it is needed |
|-----------|-----------------|
| `storage` | Store your UUID locally so your identity persists across browser sessions |
| `activeTab` | Read the text of the current tab to compute your interest vector |
| `tabs` | Detect page navigation in single-page apps so the field updates as you browse |
| `host_permissions: space.gold3112.online` | Send interest vectors to and receive field state from the Golden Protocol server |

No permission is used beyond its stated purpose.

---

## Data deletion

To delete all your data:
1. Uninstall the extension — this removes your UUID from `chrome.storage.local`
2. Your server-side interest vector and encounter history will expire automatically within 30 days

To request immediate server-side deletion, open an issue at:
[github.com/gold3112/golden-protocol/issues](https://github.com/gold3112/golden-protocol/issues)

---

## Contact

Questions or concerns: open an issue at the repository above.
