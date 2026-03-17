// popup.js — 空間はポーリングしない。空間があなたに届く。

const SERVER = 'https://space.gold3112.online';
let sse = null;
let arrived = false;

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function renderVoices(messages) {
  const section = document.getElementById('voices');
  const list    = document.getElementById('voices-list');
  if (!messages || messages.length === 0) {
    section.classList.add('hidden');
    return;
  }
  // 声は著者も時刻も出さない — 空間に漂う言葉として
  list.innerHTML = messages
    .map(m => `<li><span class="voice-text">${escapeHtml(m.text)}</span></li>`)
    .join('');
  section.classList.remove('hidden');
}

function render(field) {
  // presence: 最初に目に入るもの — 他者がここにいる
  document.getElementById('presence-count').textContent = field.presence ?? 0;

  // horizon: near より先に描く（奥行きは未解像のものから始まる）
  const horizonList = document.getElementById('horizon-list');
  horizonList.innerHTML = '';
  (field.horizon || []).slice(0, 5).forEach(e => {
    const li = document.createElement('li');
    li.innerHTML = `<span class="dot"></span><span>${escapeHtml(e.label)}</span>`;
    horizonList.appendChild(li);
  });
  if (!field.horizon || field.horizon.length === 0) {
    horizonList.innerHTML = '<li class="empty">—</li>';
  }

  // near: horizonの後に解像する
  const nearList = document.getElementById('near-list');
  nearList.innerHTML = '';
  (field.near || []).forEach(e => {
    const fillPct = ((1 - e.distance) * 100).toFixed(0);
    const li = document.createElement('li');
    li.innerHTML =
      `<span class="dot"></span>` +
      `<span class="label">${escapeHtml(e.label)}</span>` +
      `<span class="bar"><span class="bar-fill" style="width:${fillPct}%"></span></span>`;
    nearList.appendChild(li);
  });
  if (!field.near || field.near.length === 0) {
    nearList.innerHTML = '<li class="empty">—</li>';
  }

  // drift
  const driftList = document.getElementById('drift-list');
  driftList.innerHTML = '';
  (field.drift || []).forEach(d => {
    const li = document.createElement('li');
    li.innerHTML =
      `<span class="arrow">${'›'.repeat(Math.ceil(d.strength * 3))}</span>` +
      `<span>${escapeHtml(d.toward)}</span>`;
    driftList.appendChild(li);
  });
  if (!field.drift || field.drift.length === 0) {
    driftList.innerHTML = '<li class="empty">—</li>';
  }

  // voices: 空間に残された声
  renderVoices(field.messages);

  // position: 最後に浮かび上がる — あなたがどこにいたかわかる
  document.getElementById('position').textContent = field.position || '—';

  const st = document.getElementById('status');
  st.textContent = 'connected';
  st.className   = 'connected';

  // 到着シーケンス: 一度だけ
  if (!arrived) {
    arrived = true;
    arrive();
  }
}

function arrive() {
  // presence は最初から見える（arrival div は opacity: 1）
  // horizon/near/drift は少し遅れて現れる
  setTimeout(() => {
    document.getElementById('field').classList.remove('layer-hidden');
  }, 500);
  // position は最後に静かに浮かぶ
  setTimeout(() => {
    document.getElementById('position').classList.add('revealed');
  }, 1100);
}

function handleSpaceEvent(evt) {
  // 空間イベントを footer に一時的に表示
  const st = document.getElementById('status');
  const original = st.textContent;
  const originalClass = st.className;

  const messages = {
      emergence:   `${evt.label} appeared`,
      convergence: `${evt.label} — ${evt.detail || 'gathering'}`,
      encounter:   `${evt.label} is near`,
  };
  const text = messages[evt.kind] || evt.kind;

  st.textContent = text;
  st.className = 'event';
  setTimeout(() => {
      st.textContent = original;
      st.className = originalClass;
  }, 4000);
}

async function connect() {
  try {
    const [local, session] = await Promise.all([
      chrome.storage.local.get('userId'),
      chrome.storage.session.get(['pageText']),
    ]);

    const params = new URLSearchParams({
      interest: session.pageText || 'curiosity exploration encounter',
      passive:  'true',
    });
    // nameはuserIdの先頭8文字 (安定した匿名識別子)
    if (local.userId) {
        params.set('user_id', local.userId);
        params.set('name', 'wanderer_' + local.userId.slice(0, 8));
    }

    if (sse) sse.close();

    // ポーリングではなくSSE — 空間がこちらに届く
    // /field/stream への接続はユーザーを「存在している」状態として登録する
    sse = new EventSource(`${SERVER}/field/stream?${params}`);

    sse.addEventListener('field', (e) => {
      try {
        const field = JSON.parse(e.data);
        render(field);
      } catch (_) {}
    });

    sse.addEventListener('space', (e) => {
      try {
          const evt = JSON.parse(e.data);
          handleSpaceEvent(evt);
      } catch (_) {}
    });

    sse.onerror = () => {
      const st = document.getElementById('status');
      st.textContent = 'no signal';
      st.className   = 'error';
    };

  } catch (_) {
    const st = document.getElementById('status');
    st.textContent = 'no signal';
    st.className   = 'error';
  }
}

connect();
window.addEventListener('unload', () => { if (sse) sse.close(); });
