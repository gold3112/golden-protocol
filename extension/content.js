// content.js — chrome API が使えない iframe 等でも安全に動作する

function extractPageText() {
  const title = document.title || '';
  const metaDesc = document.querySelector('meta[name="description"]');
  const description = metaDesc ? metaDesc.getAttribute('content') || '' : '';
  const skip = new Set(['SCRIPT', 'STYLE', 'NAV', 'FOOTER', 'HEADER', 'ASIDE']);
  function walkText(node) {
    if (skip.has(node.nodeName)) return '';
    if (node.nodeType === Node.TEXT_NODE) return node.textContent.trim();
    return Array.from(node.childNodes).map(walkText).join(' ');
  }
  const bodyText = walkText(document.body || document.documentElement);
  return `${title} ${description} ${bodyText}`.replace(/\s+/g, ' ').trim().slice(0, 600);
}

function save() {
  // chrome API が使えない環境 (一部 iframe 等) では何もしない
  if (typeof chrome === 'undefined' || !chrome.storage?.session) return;
  const text = extractPageText();
  if (!text) return;
  try {
    chrome.storage.session.set({ pageText: text, pageUrl: location.href });
    // background.js に通知 — ポップアップが閉じていても passive 吸収が走る
    chrome.runtime.sendMessage({ type: 'PAGE_CONTEXT', text, title: document.title, url: location.href });
  } catch (_) {}
}

save();

// SPA 対応: URL 変化を監視
let lastUrl = location.href;
new MutationObserver(() => {
  if (location.href !== lastUrl) {
    lastUrl = location.href;
    setTimeout(save, 800);
  }
}).observe(document.documentElement, { subtree: true, childList: true });
