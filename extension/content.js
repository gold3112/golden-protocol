// content.js — ページのテキストを抽出して background に送信する
// SPA のURL変化にも対応

function extractPageText() {
  const title = document.title || '';

  const metaDesc = document.querySelector('meta[name="description"]');
  const description = metaDesc ? metaDesc.getAttribute('content') || '' : '';

  // body テキスト抽出 (script/style/nav/footer を除く)
  const skip = new Set(['SCRIPT', 'STYLE', 'NAV', 'FOOTER', 'HEADER', 'ASIDE']);
  function walkText(node) {
    if (skip.has(node.nodeName)) return '';
    if (node.nodeType === Node.TEXT_NODE) return node.textContent.trim();
    return Array.from(node.childNodes).map(walkText).join(' ');
  }
  const bodyText = walkText(document.body || document.documentElement);

  // 結合して先頭 600 文字
  const combined = `${title} ${description} ${bodyText}`.replace(/\s+/g, ' ').trim();
  return combined.slice(0, 600);
}

function sendToBackground() {
  const text = extractPageText();
  if (!text) return;
  chrome.runtime.sendMessage({
    type:     'PAGE_CONTEXT',
    text:     text,
    url:      location.href,
    title:    document.title,
  }).catch(() => {
    // background が起動していない場合は無視
  });
}

// 初回送信
sendToBackground();

// SPA 対応: URL 変化を監視
let lastUrl = location.href;
const observer = new MutationObserver(() => {
  if (location.href !== lastUrl) {
    lastUrl = location.href;
    setTimeout(sendToBackground, 800); // DOM 更新を待つ
  }
});
observer.observe(document.documentElement, { subtree: true, childList: true });
