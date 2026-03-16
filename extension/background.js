// background.js — SSE 接続管理 + identity 永続化
// Service Worker として動作する

const SERVER = 'https://space.gold3112.online';
const RECONNECT_DELAY = 3000; // ms

let eventSource  = null;
let currentField = null;
let userId       = null;
let currentText  = '';

// --- identity 管理 ---

async function getUserId() {
  if (userId) return userId;
  const stored = await chrome.storage.local.get('userId');
  if (stored.userId) {
    userId = stored.userId;
    return userId;
  }
  // 新規作成
  try {
    const res = await fetch(`${SERVER}/identity/new?interest=curiosity+exploration+encounter`);
    const data = await res.json();
    userId = data.id;
    await chrome.storage.local.set({ userId });
    return userId;
  } catch {
    // サーバー未起動時はランダム UUID を一時使用
    userId = crypto.randomUUID();
    return userId;
  }
}

// --- SSE 接続 ---

async function connect(interestText) {
  if (eventSource) {
    eventSource.close();
    eventSource = null;
  }

  const id     = await getUserId();
  const params = new URLSearchParams({
    user_id:  id,
    interest: interestText || 'curiosity exploration encounter',
    passive:  'true',
  });
  const url = `${SERVER}/field/stream?${params}`;

  eventSource = new EventSource(url);

  eventSource.addEventListener('field', (e) => {
    try {
      currentField = JSON.parse(e.data);
      // popup が開いていれば通知
      chrome.runtime.sendMessage({ type: 'FIELD_UPDATE', field: currentField }).catch(() => {});
    } catch {}
  });

  eventSource.onerror = () => {
    eventSource.close();
    eventSource = null;
    setTimeout(() => connect(currentText), RECONNECT_DELAY);
  };
}

// --- メッセージハンドラ ---

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg.type === 'PAGE_CONTEXT') {
    currentText = msg.text;
    // ページ遷移のたびに interest を更新して再接続
    connect(msg.text);
    sendResponse({ ok: true });
  }

  if (msg.type === 'GET_FIELD') {
    sendResponse({ field: currentField, userId });
  }

  return true; // async response
});

// --- 起動時接続 ---
connect('');
