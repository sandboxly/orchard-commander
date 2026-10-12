(() => {
  // File-list mock-ups: click or arrow keys move the cursor, the preview follows.
  document.querySelectorAll('[data-filelist]').forEach((app) => {
    const list = app.querySelector('.files');
    const rows = [...list.querySelectorAll('li')];
    const title = app.querySelector('[data-preview-title]');
    const body = app.querySelector('[data-preview]');
    const meta = app.querySelector('[data-preview-meta]');
    let cur = rows.findIndex((r) => r.getAttribute('aria-selected') === 'true');
    if (cur < 0) cur = 0;

    const show = (i) => {
      cur = Math.max(0, Math.min(rows.length - 1, i));
      rows.forEach((r, n) => r.setAttribute('aria-selected', String(n === cur)));
      const r = rows[cur];
      list.setAttribute('aria-activedescendant', r.id);
      const name = r.dataset.name;
      if (title) title.textContent = name === '..' ? 'Preview' : name;
      if (r.dataset.src) {
        body.innerHTML = '';
        const img = new Image();
        img.src = r.dataset.src;
        img.alt = r.dataset.alt || '';
        body.appendChild(img);
        if (meta) meta.textContent = r.dataset.meta;
      } else {
        body.innerHTML = '<div class="folder-info"><i class="ph ph-folder" aria-hidden="true"></i><span></span></div>';
        body.querySelector('span').textContent = name === '..' ? 'Parent folder' : name + '/ · ' + r.dataset.items + ' items';
        if (meta) meta.textContent = name === '..' ? '' : 'Folder';
      }
      const top = r.offsetTop - list.offsetTop;
      if (top < list.scrollTop || top + r.offsetHeight > list.scrollTop + list.clientHeight) list.scrollTop = top;
    };

    rows.forEach((r, n) => r.addEventListener('click', () => { show(n); list.focus({ preventScroll: true }); }));
    list.addEventListener('keydown', (e) => {
      const step = { ArrowDown: 1, ArrowUp: -1, j: 1, k: -1 }[e.key];
      if (step) { e.preventDefault(); show(cur + step); }
      else if (e.key === 'Home') { e.preventDefault(); show(0); }
      else if (e.key === 'End') { e.preventDefault(); show(rows.length - 1); }
    });
    list.addEventListener('focus', () => app.querySelector('.pane')?.classList.add('focus'));
    list.addEventListener('blur', () => app.querySelector('.pane')?.classList.remove('focus'));
    show(cur);
  });

  // Highlight the visitor's platform.
  const ua = (navigator.userAgentData?.platform || navigator.platform || navigator.userAgent || '').toLowerCase();
  const os = ua.includes('win') ? 'windows' : (ua.includes('linux') || ua.includes('x11')) && !ua.includes('android') ? 'linux' : ua.includes('mac') ? 'macos' : null;
  if (os) {
    const card = document.querySelector(`.dl[data-os="${os}"]`);
    card?.classList.add('active');
    const hero = document.querySelector('[data-hero-dl]');
    const primary = card?.querySelector('[data-primary]');
    if (hero && primary) {
      hero.href = primary.href;
      hero.querySelector('span').textContent = 'Download for ' + card.dataset.label;
    }
  }

  // Copy buttons.
  document.querySelectorAll('[data-copy]').forEach((b) => {
    b.addEventListener('click', async () => {
      const text = document.getElementById(b.dataset.copy).textContent.trim();
      try { await navigator.clipboard.writeText(text); } catch { return; }
      const label = b.innerHTML;
      b.classList.add('ok');
      b.innerHTML = '<i class="ph ph-check" aria-hidden="true"></i><span class="sr">Copied</span>';
      b.setAttribute('aria-label', 'Copied');
      setTimeout(() => { b.classList.remove('ok'); b.innerHTML = label; b.setAttribute('aria-label', 'Copy command'); }, 1600);
    });
  });

  // Screenshot lightbox.
  const box = document.getElementById('lightbox');
  if (box) {
    const img = box.querySelector('img');
    const chip = box.querySelector('.chip');
    document.querySelectorAll('.shot').forEach((s) => s.addEventListener('click', () => {
      const src = s.querySelector('img');
      img.src = src.currentSrc || src.src;
      img.alt = src.alt;
      chip.textContent = s.dataset.name || '';
      box.showModal();
    }));
    box.addEventListener('click', (e) => { if (e.target === box || e.target.closest('.close')) box.close(); });
  }

  // Manual: collapse chapter list on narrow screens.
  const toc = document.querySelector('.toc details');
  if (toc && matchMedia('(max-width: 860px)').matches) toc.open = false;
})();
