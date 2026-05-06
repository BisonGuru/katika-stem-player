// Vanilla JS — no bundler. Tauri 2 injects window.__TAURI__ globally when
// `app.withGlobalTauri = true` (set in tauri.conf.json).
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel) => document.querySelector(sel);

// ---------- Activity log ----------
const log = (msg, kind = "info") => {
  const ts = new Date().toLocaleTimeString();
  const line = `[${ts}] [${kind}] ${msg}\n`;
  for (const sel of ["#log", "#log-debug"]) {
    const el = document.querySelector(sel);
    if (!el) continue;
    el.textContent += line;
    el.scrollTop = el.scrollHeight;
  }
};

const setStatus = (text, cls) => {
  const el = $("#status");
  el.textContent = text;
  el.className = `status ${cls}`;
};

// ---------- App state ----------
let pickedPath = null;
let isConnected = false;
let demucsAvailable = false;
let highlightSlot = null;

const AUDIO_EXTS = ["mp3", "wav", "flac", "m4a", "aac", "ogg", "aiff"];
const isAudioFile = (path) =>
  AUDIO_EXTS.includes((path.split(".").pop() || "").toLowerCase());

// Built-in slot labels for the four shipped Kanye albums + the on-device
// recording slot. Anything else is a user-upload slot.
const BUILTIN_LABELS = {
  A1: "Jesus Is King",
  A2: "Donda",
  A3: "Life of the Party",
  A4: "Donda 2",
  RECORD: "Recording",
};

// Map a raw track id ("t1", "T2", "T13") to a friendly track number.
const trackNumber = (raw) => {
  const m = (raw || "").match(/(\d+)/);
  return m ? parseInt(m[1], 10) : null;
};

// Tiny HTML escaper for the bits we splice into innerHTML.
const escapeHtml = (s) =>
  String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");

// ---------- UI state transitions ----------
const setConnectedUI = (info) => {
  isConnected = true;
  $("#connect").hidden = true;
  $("#disconnect").hidden = false;
  $("#menu-toggle").hidden = false;
  $("#add-album").hidden = false;
  $("#read-library").hidden = false;
  $("#drop-zone").disabled = false;
  $("#library-empty").hidden = true;

  setStatus(`connected · ${info.product ?? "Stem Player"}`, "connected");
  refreshAddButton();
  loadLibrary().catch((e) => log(`initial library load failed: ${e}`, "err"));
};

const setDisconnectedUI = () => {
  isConnected = false;
  $("#connect").hidden = false;
  $("#disconnect").hidden = true;
  $("#menu-toggle").hidden = true;
  $("#menu").hidden = true;
  $("#add-album").hidden = true;
  $("#read-library").hidden = true;
  $("#drop-zone").disabled = true;
  $("#albums").innerHTML = "";
  $("#library-empty").hidden = false;
  setStatus("disconnected", "disconnected");
  refreshAddButton();
};

// ---------- Library rendering ----------
const renderLibraryPlaceholder = (msg) => {
  const root = $("#albums");
  root.innerHTML = `<div class="muted small lib-msg">${msg}</div>`;
};

// Sort albums in a friendly order: built-ins first (A1..A4, RECORD),
// then user uploads (A5+) in numeric order.
const sortAlbumsBy = (albums, idField) => {
  const order = (id) => {
    if (id === "A1") return 1;
    if (id === "A2") return 2;
    if (id === "A3") return 3;
    if (id === "A4") return 4;
    if (id === "RECORD") return 5;
    const m = id?.match(/^A(\d+)$/);
    if (m) return 100 + parseInt(m[1], 10);
    return 9999;
  };
  return [...albums].sort((a, b) => order(a[idField]) - order(b[idField]));
};

// Render the library from an enriched payload from the Rust read_library
// command. Each entry has `{id, title, artist, tracks}`. We fall back to
// our static built-in labels and a "Custom album" placeholder.
const renderLibrary = (lib) => {
  const root = $("#albums");
  root.innerHTML = "";

  const list = Array.isArray(lib?.albums) ? lib.albums : [];
  if (list.length === 0) {
    root.innerHTML = `<div class="muted small lib-msg">No albums on device.</div>`;
    return;
  }

  for (const album of sortAlbumsBy(list, "id")) {
    const id = album.id ?? "?";
    const tracks = Array.isArray(album.tracks) ? album.tracks : [];
    const isBuiltin = id in BUILTIN_LABELS || id === "RECORD";
    const isUserUpload = !isBuiltin;
    const fromConfig = album.title && album.title.trim().length > 0
      ? album.title
      : null;
    const label = isBuiltin
      ? (BUILTIN_LABELS[id] ?? "Album")
      : (fromConfig ?? "Custom album");
    const subtitle = album.artist && album.artist !== label ? album.artist : null;

    const row = document.createElement("div");
    row.className = "album-row";
    if (isBuiltin) row.classList.add("is-builtin");
    if (id === highlightSlot) row.classList.add("is-new");

    // Left column: small disc icon (built-in) or letter mark (user upload)
    const mark = document.createElement("div");
    mark.className = "album-mark";
    if (isBuiltin) {
      mark.classList.add("disc");
    } else {
      mark.classList.add("user");
      mark.textContent = id.replace(/^A/, "");
    }
    row.appendChild(mark);

    // Title + slot ID
    const titleWrap = document.createElement("div");
    titleWrap.className = "album-titles";
    const trackBit = tracks.length === 0
      ? "no tracks"
      : `${tracks.length} track${tracks.length === 1 ? "" : "s"}`;
    const metaParts = subtitle
      ? `${subtitle} · ${id} · ${trackBit}`
      : `${id} · ${trackBit}`;
    titleWrap.innerHTML = `
      <div class="album-name">${escapeHtml(label)}</div>
      <div class="album-meta muted small">${escapeHtml(metaParts)}</div>
    `;
    row.appendChild(titleWrap);

    // Track chips
    const chipWrap = document.createElement("div");
    chipWrap.className = "track-chips";
    if (tracks.length === 0) {
      const empty = document.createElement("span");
      empty.className = "muted small chip-empty";
      empty.textContent = isBuiltin ? "Empty" : "No tracks yet";
      chipWrap.appendChild(empty);
    } else {
      for (const tr of tracks) {
        const tid = tr.t ?? "?";
        const num = trackNumber(tid);
        const chip = document.createElement("span");
        chip.className = "track-chip";

        const name = document.createElement("span");
        name.className = "track-chip-name";
        name.textContent = num != null ? `Track ${num}` : tid;
        chip.appendChild(name);

        const del = document.createElement("button");
        del.className = "track-chip-del";
        del.textContent = "×";
        del.title = "Remove track";
        del.addEventListener("click", async (e) => {
          e.stopPropagation();
          del.disabled = true;
          try {
            log(`deleting track ${id}/${tid} …`);
            await invoke("delete_track", { album: id, track: tid });
            await loadLibrary();
          } catch (err) {
            log(`delete track failed: ${err}`, "err");
            del.disabled = false;
          }
        });
        chip.appendChild(del);

        chipWrap.appendChild(chip);
      }
    }
    row.appendChild(chipWrap);

    // Right-side actions
    const actions = document.createElement("div");
    actions.className = "album-row-actions";
    if (isUserUpload) {
      const delAlbum = document.createElement("button");
      delAlbum.className = "row-action-btn icon-btn";
      delAlbum.textContent = "🗑";
      delAlbum.title = "Delete album";
      delAlbum.addEventListener("click", async () => {
        const ok = tracks.length === 0
          ? true
          : confirm(`Delete this album? This will remove ${tracks.length} track(s).`);
        if (!ok) return;
        delAlbum.disabled = true;
        try {
          log(`deleting album ${id} …`);
          await invoke("delete_album", { album: id });
          await loadLibrary();
        } catch (err) {
          log(`delete album failed: ${err}`, "err");
          delAlbum.disabled = false;
        }
      });
      actions.appendChild(delAlbum);
    }
    row.appendChild(actions);

    root.appendChild(row);
  }

  // Drop the new-album highlight after a short while.
  if (highlightSlot) {
    const target = highlightSlot;
    setTimeout(() => {
      if (highlightSlot === target) {
        highlightSlot = null;
        document
          .querySelectorAll(".album-row.is-new")
          .forEach((el) => el.classList.remove("is-new"));
      }
    }, 2500);
  }
};

async function loadLibrary() {
  if (!isConnected) return;
  renderLibraryPlaceholder("loading…");
  try {
    const lib = await invoke("read_library");
    renderLibrary(lib);
    const slotCount = Array.isArray(lib?.albums) ? lib.albums.length : 0;
    log(`library: ${slotCount} slot(s)`);
  } catch (e) {
    log(`library fetch failed: ${e}`, "err");
    renderLibraryPlaceholder(`library fetch failed: ${e}`);
  }
}

// ---------- Add song flow ----------
const refreshAddButton = () => {
  const btn = $("#add-go");
  const hint = $("#add-hint");
  if (!btn) return;

  if (!isConnected) {
    btn.disabled = true;
    hint.textContent = "";
    return;
  }
  if (!pickedPath) {
    btn.disabled = true;
    hint.textContent = "";
    return;
  }
  if (isAudioFile(pickedPath)) {
    if (!demucsAvailable) {
      btn.disabled = true;
      btn.textContent = "Add to device";
      hint.textContent =
        "demucs not installed — install via `pip3 install --user demucs` to upload songs";
      return;
    }
    btn.disabled = false;
    btn.textContent = "Add to device";
    hint.textContent = "Splits into 4 stems locally, then uploads (~30–60s).";
  } else {
    btn.disabled = true;
    btn.textContent = "Add to device";
    hint.textContent = "Pick an audio file (mp3, wav, flac, m4a, aac, ogg).";
  }
};

const setPickedFile = (path) => {
  pickedPath = path;
  const base = path.split("/").pop() || path;
  $("#picked-name").textContent = base;
  $("#picked-path").textContent = path;
  $("#picked-row").hidden = false;
  refreshAddButton();
};

const clearPickedFile = () => {
  pickedPath = null;
  $("#picked-row").hidden = true;
  refreshAddButton();
};

async function pickFile() {
  return await invoke("plugin:dialog|open", {
    options: {
      multiple: false,
      filters: [{ name: "Audio", extensions: AUDIO_EXTS }],
    },
  });
}

// ---------- Wire buttons ----------
$("#connect").addEventListener("click", async () => {
  setStatus("connecting…", "busy");
  try {
    const info = await invoke("connect");
    log(`connected to ${info.product ?? "device"} (sn=${info.serial ?? "?"})`);
    setConnectedUI(info);
  } catch (e) {
    log(`connect failed: ${e}`, "err");
    setStatus("disconnected", "disconnected");
  }
});

$("#disconnect").addEventListener("click", async () => {
  await invoke("disconnect");
  log("disconnected");
  setDisconnectedUI();
});

// Header overflow menu (Reboot / Version)
$("#menu-toggle").addEventListener("click", (e) => {
  e.stopPropagation();
  $("#menu").hidden = !$("#menu").hidden;
});
document.addEventListener("click", (e) => {
  const menu = $("#menu");
  if (menu.hidden) return;
  if (!menu.contains(e.target) && e.target.id !== "menu-toggle") {
    menu.hidden = true;
  }
});

$("#ping").addEventListener("click", async () => {
  $("#menu").hidden = true;
  try {
    const resp = await invoke("send_cmd", { sub: 0x01, payload: null });
    const ascii = resp
      .map((b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : "."))
      .join("");
    log(`firmware version: ${ascii}`);
  } catch (e) {
    log(`version query failed: ${e}`, "err");
  }
});

$("#reboot").addEventListener("click", async () => {
  $("#menu").hidden = true;
  if (!confirm("Reboot the device? It will disconnect and reappear after a few seconds.")) {
    return;
  }
  log("rebooting device…");
  try {
    await invoke("reboot_device");
    log("reboot sent. device is reconnecting…");
  } catch (e) {
    log(`reboot returned: ${e}`, "info");
  } finally {
    setDisconnectedUI();
  }
});

$("#read-library").addEventListener("click", () => loadLibrary());

$("#add-album").addEventListener("click", async () => {
  const btn = $("#add-album");
  btn.disabled = true;
  const orig = btn.textContent;
  btn.textContent = "creating…";
  try {
    const slot = await invoke("add_album");
    log(`new album created at ${slot}`);
    highlightSlot = slot;
    await loadLibrary();
  } catch (e) {
    log(`add album failed: ${e}`, "err");
  } finally {
    btn.textContent = orig;
    btn.disabled = false;
  }
});

$("#drop-zone").addEventListener("click", async () => {
  try {
    const p = await pickFile();
    if (!p) return;
    setPickedFile(p);
  } catch (e) {
    log(`file picker failed: ${e}`, "err");
  }
});

$("#picked-clear").addEventListener("click", clearPickedFile);

// ---------- Progress bar ----------
const showProgress = (label = "Working…", pct = 0) => {
  $("#progress").hidden = false;
  $("#progress").classList.remove("indeterminate");
  $("#progress-label").textContent = label;
  $("#progress-pct").textContent = `${Math.round(pct)}%`;
  $("#progress-fill").style.width = `${Math.max(0, Math.min(100, pct))}%`;
};
const hideProgress = () => {
  $("#progress").hidden = true;
  $("#progress").classList.remove("indeterminate");
  $("#progress-fill").style.width = "0%";
};

// Subscribe once on boot so progress events drive the bar regardless of
// which command kicked them off.
listen("upload-progress", (e) => {
  const { stage, pct, message } = e.payload || {};
  showProgress(message || stage || "Working…", typeof pct === "number" ? pct : 0);
}).catch(() => {});

$("#add-go").addEventListener("click", async () => {
  if (!pickedPath || !isAudioFile(pickedPath)) return;
  const btn = $("#add-go");
  const hint = $("#add-hint");
  btn.disabled = true;
  btn.textContent = "Working…";
  hint.textContent = "";
  showProgress("Starting…", 1);
  log(`adding ${pickedPath.split("/").pop()} to device…`);
  try {
    const result = await invoke("split_and_push", { path: pickedPath });
    log(`added "${result.track_name}" to device`);
    showProgress("Done", 100);
    setTimeout(hideProgress, 700);
    clearPickedFile();
    await loadLibrary();
  } catch (e) {
    log(`add failed: ${e}`, "err");
    hideProgress();
  } finally {
    btn.textContent = "Add to device";
    refreshAddButton();
  }
});

// ---------- Drag-drop support ----------
async function setupDragDrop() {
  const zone = $("#drop-zone");
  window.addEventListener("dragover", (e) => e.preventDefault());
  window.addEventListener("drop", (e) => e.preventDefault());

  const showHover = () => zone.classList.add("dragging");
  const hideHover = () => zone.classList.remove("dragging");

  await listen("tauri://drag-enter", showHover).catch(() => {});
  await listen("tauri://drag-over", showHover).catch(() => {});
  await listen("tauri://file-drop-hover", showHover).catch(() => {});
  await listen("tauri://drag-leave", hideHover).catch(() => {});
  await listen("tauri://file-drop-cancelled", hideHover).catch(() => {});

  const handleDrop = (event) => {
    hideHover();
    const payload = event.payload;
    let paths = [];
    if (Array.isArray(payload)) paths = payload;
    else if (payload && Array.isArray(payload.paths)) paths = payload.paths;
    if (!paths.length) return;
    setPickedFile(paths[0]);
    log(`dropped: ${paths[0]}`);
  };

  await listen("tauri://drag-drop", handleDrop).catch(() => {});
  await listen("tauri://file-drop", handleDrop).catch(() => {});
}

// ---------- Boot ----------
(async () => {
  try {
    if (!window.__TAURI__ || !window.__TAURI__.core) {
      log("Tauri global not available", "err");
      return;
    }
    setupDragDrop().catch((e) => log(`drag-drop init failed: ${e}`, "err"));

    // Demucs probe → small status pill.
    try {
      const demucsPath = await invoke("check_demucs");
      demucsAvailable = demucsPath !== null;
      const pill = $("#demucs-pill");
      if (demucsAvailable) {
        pill.textContent = "● stem-splitter ready";
        pill.classList.add("ok");
      } else {
        pill.textContent = "○ stem-splitter not installed";
      }
      refreshAddButton();
    } catch (e) {
      log(`demucs check failed: ${e}`, "err");
    }

    // Initial connection state.
    const connected = await invoke("is_connected");
    if (connected) {
      try {
        const info = await invoke("connect");
        setConnectedUI(info);
        log(`auto-reconnected to ${info.product ?? "device"}`);
      } catch (e) {
        log(`auto-reconnect failed: ${e}`, "err");
        setDisconnectedUI();
      }
    } else {
      setDisconnectedUI();
      log("ready. plug in your Stem Player and click Connect.");
    }
  } catch (e) {
    log(`boot failed: ${e}`, "err");
  }
})();


// ---------- Debug tab wiring ----------
(() => {
  const tabBtns = document.querySelectorAll('.tabs .tab');
  const panels = document.querySelectorAll('[data-panel]');
  if (!tabBtns.length || !panels.length) return;

  const setTab = (name) => {
    tabBtns.forEach((b) => b.classList.toggle('is-active', b.dataset.tab === name));
    panels.forEach((p) => { p.hidden = p.dataset.panel !== name; });
  };
  tabBtns.forEach((b) => b.addEventListener('click', () => setTab(b.dataset.tab)));

  const setBtn = (sel, enabled) => {
    const el = document.querySelector(sel);
    if (el) el.disabled = !enabled;
  };

  let rawPickedPath = null;

  const updateDebugButtons = () => {
    setBtn('#raw-send', isConnected);
    setBtn('#probe-refresh', isConnected);
    setBtn('#dump-refresh', isConnected);
    setBtn('#raw-drop', isConnected);
    setBtn('#raw-push', isConnected && rawPickedPath !== null);
  };

  // Re-evaluate debug button states every time the status pill mutates.
  const statusEl = document.querySelector('#status');
  if (statusEl) {
    new MutationObserver(updateDebugButtons).observe(statusEl, {
      childList: true, characterData: true, subtree: true, attributes: true
    });
  }
  updateDebugButtons();

  // ---------- Format response bytes for human inspection ----------
  const formatBytes = (bytes) => {
    const hex = bytes.map((b) => b.toString(16).padStart(2, '0')).join(' ');
    const ascii = bytes.map((b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : '.')).join('');
    let parsed = '';
    try {
      let body = bytes.slice(1);
      while (body.length && body[body.length - 1] === 0) body = body.slice(0, -1);
      const text = String.fromCharCode(...body);
      parsed = JSON.stringify(JSON.parse(text), null, 2);
    } catch (_) { /* not JSON */ }
    const out = [
      bytes.length + ' bytes',
      'hex:   ' + hex,
      'ascii: ' + ascii,
    ];
    if (parsed) out.push('parsed:\n' + parsed);
    return out.join('\n\n');
  };

  // ---------- Raw 0x04 sender ----------
  document.querySelector('#raw-send').addEventListener('click', async () => {
    const subStr = document.querySelector('#raw-sub').value.trim();
    const sub = parseInt(subStr, 16);
    if (Number.isNaN(sub) || sub < 0 || sub > 0xff) {
      log('raw send: bad sub-byte ' + subStr, 'err');
      return;
    }
    const payloadStr = document.querySelector('#raw-payload').value.trim();
    let payload = null;
    if (payloadStr) {
      try { payload = JSON.parse(payloadStr); }
      catch (e) { log('raw send: payload not valid JSON: ' + e, 'err'); return; }
    }
    const respEl = document.querySelector('#raw-resp');
    respEl.hidden = false;
    respEl.textContent = 'sending…';
    log('raw 0x04 0x' + sub.toString(16).padStart(2, '0') + (payload ? ' ' + JSON.stringify(payload) : ''));
    try {
      const resp = await invoke('send_cmd', { sub, payload });
      respEl.textContent = formatBytes(resp);
      log('raw response: ' + resp.length + ' bytes');
    } catch (e) {
      respEl.textContent = 'error: ' + e;
      log('raw send failed: ' + e, 'err');
    }
  });

  // ---------- Probe slots ----------
  const renderProbeGrid = (lib) => {
    const root = document.querySelector('#probe-grid');
    root.innerHTML = '';
    const albums = Array.isArray(lib?.albums) ? lib.albums : [];
    if (!albums.length) { root.innerHTML = '<div class="muted small">no slots</div>'; return; }
    for (const album of albums) {
      const id = album.id;
      const card = document.createElement('div');
      card.className = 'probe-card';
      card.innerHTML = '<div class="probe-head"><span class="slot">' + id + '</span><button class="ghost small probe-btn">Probe</button></div><pre class="probe-resp raw-resp" hidden></pre>';
      const btn = card.querySelector('.probe-btn');
      const respEl = card.querySelector('.probe-resp');
      btn.addEventListener('click', async () => {
        btn.disabled = true; btn.textContent = '…';
        respEl.hidden = false; respEl.textContent = 'probing…';
        try {
          const resp = await invoke('send_cmd', { sub: 0x05, payload: { album: id } });
          respEl.textContent = formatBytes(resp);
          log('probe ' + id + ': ' + resp.length + ' bytes');
        } catch (e) {
          respEl.textContent = 'error: ' + e;
          log('probe ' + id + ' failed: ' + e, 'err');
        } finally {
          btn.disabled = false; btn.textContent = 'Probe';
        }
      });
      root.appendChild(card);
    }
  };

  document.querySelector('#probe-refresh').addEventListener('click', async () => {
    try {
      const lib = await invoke('read_library');
      renderProbeGrid(lib);
    } catch (e) {
      log('probe-refresh failed: ' + e, 'err');
    }
  });

  // ---------- Library dump ----------
  document.querySelector('#dump-refresh').addEventListener('click', async () => {
    const respEl = document.querySelector('#dump-resp');
    respEl.hidden = false; respEl.textContent = 'querying device…';
    try {
      const resp = await invoke('send_cmd', { sub: 0x03, payload: null });
      respEl.textContent = formatBytes(resp);
      log('library dump: ' + resp.length + ' bytes');
    } catch (e) {
      respEl.textContent = 'error: ' + e;
      log('library dump failed: ' + e, 'err');
    }
  });

  // ---------- Raw push ----------
  document.querySelector('#raw-drop').addEventListener('click', async () => {
    try {
      const p = await invoke('plugin:dialog|open', { options: { multiple: false, filters: [] } });
      if (!p) return;
      rawPickedPath = p;
      document.querySelector('#raw-picked').textContent = p;
      const base = p.split('/').pop();
      if (base) document.querySelector('#raw-name').value = base;
      updateDebugButtons();
    } catch (e) { log('raw file pick failed: ' + e, 'err'); }
  });

  document.querySelector('#raw-push').addEventListener('click', async () => {
    if (!rawPickedPath) return;
    const fileType = document.querySelector('#raw-type').value;
    const name = document.querySelector('#raw-name').value || 'untitled';
    log('raw push: ' + name + ' (type=' + fileType + ')');
    try {
      await invoke('push_file_cmd', { path: rawPickedPath, fileType, name });
      log('raw push complete: ' + name);
    } catch (e) {
      log('raw push failed: ' + e, 'err');
    }
  });

  // ---------- Clear log ----------
  document.querySelector('#log-clear').addEventListener('click', () => {
    for (const sel of ['#log', '#log-debug']) {
      const el = document.querySelector(sel);
      if (el) el.textContent = '';
    }
  });
})();
