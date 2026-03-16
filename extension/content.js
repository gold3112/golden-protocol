// content.js — ページテキストを抽出して chrome.storage.session に直接書く

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
  const text = extractPageText();
  if (!text) return;
  // background を経由せず直接 session storage に書く
  chrome.storage.session.set({ pageText: text, pageUrl: location.href });
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
