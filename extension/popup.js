// popup.js — popup が開いている間だけ /field を直接ポーリング

const SERVER = 'https://space.gold3112.online';
let pollTimer = null;

function render(field) {
  document.getElementById('position').textContent = field.position || '—';
  document.getElementById('density').textContent  = `density ${((field.density || 0) * 100).toFixed(0)}%`;

  // near
  const nearList = document.getElementById('near-list');
  nearList.innerHTML = '';
  (field.near || []).forEach(e => {
    const fillPct = ((1 - e.distance) * 100).toFixed(0);
    const li = document.createElement('li');
    li.innerHTML = `
      <span class="dot"></span>
      <span class="label">${e.label}</span>
      <span class="bar"><span class="bar-fill" style="width:${fillPct}%"></span></span>`;
    nearList.appendChild(li);
  });
  if (!field.near || field.near.length === 0) {
    nearList.innerHTML = '<li class="empty">nothing near</li>';
  }

  // horizon
  const horizonList = document.getElementById('horizon-list');
  horizonList.innerHTML = '';
  (field.horizon || []).slice(0, 5).forEach(e => {
    const li = document.createElement('li');
    li.innerHTML = `<span class="dot"></span><span>${e.label}</span>`;
    horizonList.appendChild(li);
  });
  if (!field.horizon || field.horizon.length === 0) {
    horizonList.innerHTML = '<li class="empty">—</li>';
  }

  // drift
  const driftList = document.getElementById('drift-list');
  driftList.innerHTML = '';
  (field.drift || []).forEach(d => {
    const li = document.createElement('li');
    li.innerHTML = `<span class="arrow">${'›'.repeat(Math.ceil(d.strength * 3))}</span><span>${d.toward}</span>`;
    driftList.appendChild(li);
  });
  if (!field.drift || field.drift.length === 0) {
    driftList.innerHTML = '<li class="empty">—</li>';
  }

  document.getElementById('presence').textContent = `${field.presence || 0} present`;
  const st = document.getElementById('status');
  st.textContent = 'connected';
  st.className   = 'connected';
}

async function fetchAndRender() {
  try {
    const [local, session] = await Promise.all([
      chrome.storage.local.get('userId'),
      chrome.storage.session.get(['pageText']),
    ]);

    const params = new URLSearchParams({
      interest: session.pageText || 'curiosity exploration encounter',
      passive:  'true',
    });
    if (local.userId) params.set('user_id', local.userId);

    const res   = await fetch(`${SERVER}/field?${params}`);
    const field = await res.json();
    render(field);
  } catch {
    const st = document.getElementById('status');
    st.textContent = 'no signal';
    st.className   = 'error';
  }
}

// 開いた瞬間に取得 + 3秒ごとに更新
fetchAndRender();
pollTimer = setInterval(fetchAndRender, 3000);
window.addEventListener('unload', () => clearInterval(pollTimer));
