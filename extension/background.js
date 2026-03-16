// background.js — identity 管理のみ (SSE なし)
// MV3 の service worker は短命なので SSE は持たない

const SERVER = 'https://space.gold3112.online';

async function getUserId() {
  const stored = await chrome.storage.local.get('userId');
  if (stored.userId) return stored.userId;

  try {
    const res  = await fetch(`${SERVER}/identity/new?interest=curiosity+exploration+encounter`);
    const data = await res.json();
    await chrome.storage.local.set({ userId: data.id });
    return data.id;
  } catch {
    // サーバー未起動時は一時 UUID
    const id = crypto.randomUUID();
    await chrome.storage.local.set({ userId: id });
    return id;
  }
}

// インストール時に identity を作成
chrome.runtime.onInstalled.addListener(() => getUserId());

// メッセージハンドラ
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === 'PAGE_CONTEXT') {
    // ページテキストを session storage に保存 (popup が読む)
    chrome.storage.session.set({ pageText: msg.text, pageUrl: msg.url });
    sendResponse({ ok: true });
  }
  if (msg.type === 'GET_USER_ID') {
    getUserId().then(id => sendResponse({ userId: id }));
    return true;
  }
  return true;
});
