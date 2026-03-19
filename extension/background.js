// background.js — ブラウジングが空間を育てる
// MV3 の service worker は短命なので SSE は持たない

const SERVER = 'https://space.gold3112.online';

// identity を /arrive で作成・復元
async function getOrCreateIdentity() {
  const { userId } = await chrome.storage.local.get('userId');
  try {
    const res = await fetch(`${SERVER}/arrive`, {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({ identity: userId || null }),
    });
    const data = await res.json();
    await chrome.storage.local.set({ userId: data.identity });
    return { userId: data.identity, field: data.field };
  } catch {
    const id = userId || crypto.randomUUID();
    await chrome.storage.local.set({ userId: id });
    return { userId: id, field: null };
  }
}

chrome.runtime.onInstalled.addListener(() => getOrCreateIdentity());

// ページ訪問を空間への参加に変える
async function absorbPage(text, title) {
  if (!title || title.length < 4) return;

  const { userId, field } = await getOrCreateIdentity();

  // バッジに presence 数を表示
  const presence = field?.presence ?? 0;
  chrome.action.setBadgeText({ text: presence > 0 ? String(presence) : '' });
  chrome.action.setBadgeBackgroundColor({ color: '#c8a840' });

  // 訪れたページを空間のエンティティとして宣言
  // ブラウジングが空間を育てる。誰かが同じ場所を訪れたとき、近くに現れる。
  if (text && text.length > 20) {
    fetch(`${SERVER}/entities`, {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({
        label: title.slice(0, 80),
        kind:  'Service',
        text:  text.slice(0, 300),
      }),
    }).catch(() => {});
  }
}

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === 'PAGE_CONTEXT') {
    chrome.storage.session.set({ pageText: msg.text, pageUrl: msg.url });
    absorbPage(msg.text, msg.title);
    sendResponse({ ok: true });
  }
  if (msg.type === 'GET_USER_ID') {
    getOrCreateIdentity().then(({ userId }) => sendResponse({ userId }));
    return true;
  }
  return true;
});
