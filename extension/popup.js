// popup.js — field state を受け取って描画する

function render(field) {
  if (!field) return;

  // header
  document.getElementById('position').textContent = field.position || '—';
  document.getElementById('density').textContent =
    `density ${(field.density * 100).toFixed(0)}%`;

  // near
  const nearList = document.getElementById('near-list');
  nearList.innerHTML = '';
  if (field.near && field.near.length > 0) {
    field.near.forEach(e => {
      const li = document.createElement('li');
      const fillPct = ((1 - e.distance) * 100).toFixed(0);
      li.innerHTML = `
        <span class="dot"></span>
        <span class="label">${e.label}</span>
        <span class="bar"><span class="bar-fill" style="width:${fillPct}%"></span></span>
      `;
      nearList.appendChild(li);
    });
  } else {
    nearList.innerHTML = '<li class="empty">nothing near</li>';
  }

  // horizon
  const horizonList = document.getElementById('horizon-list');
  horizonList.innerHTML = '';
  if (field.horizon && field.horizon.length > 0) {
    field.horizon.slice(0, 5).forEach(e => {
      const li = document.createElement('li');
      li.innerHTML = `<span class="dot"></span><span>${e.label}</span>`;
      horizonList.appendChild(li);
    });
  } else {
    horizonList.innerHTML = '<li class="empty">—</li>';
  }

  // drift
  const driftList = document.getElementById('drift-list');
  driftList.innerHTML = '';
  if (field.drift && field.drift.length > 0) {
    field.drift.forEach(d => {
      const li = document.createElement('li');
      const arrows = '›'.repeat(Math.ceil(d.strength * 3));
      li.innerHTML = `<span class="arrow">${arrows}</span><span>${d.toward}</span>`;
      driftList.appendChild(li);
    });
  } else {
    driftList.innerHTML = '<li class="empty">—</li>';
  }

  // footer
  document.getElementById('presence').textContent =
    `${field.presence || 0} present`;

  const statusEl = document.getElementById('status');
  statusEl.textContent = 'connected';
  statusEl.className = 'connected';
}

// background から現在の field state を取得
chrome.runtime.sendMessage({ type: 'GET_FIELD' }, (res) => {
  if (res && res.field) {
    render(res.field);
  } else {
    document.getElementById('status').textContent = 'no signal';
  }
});

// background からのリアルタイム更新を受け取る
chrome.runtime.onMessage.addListener((msg) => {
  if (msg.type === 'FIELD_UPDATE' && msg.field) {
    render(msg.field);
  }
});
