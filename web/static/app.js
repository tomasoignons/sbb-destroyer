// Canton Run web app: form → background job → saved routes → map.
(async function () {
  const $ = s => document.querySelector(s);
  const esc = RouteView.esc;
  const api = async (path, opts = {}) => {
    const r = await fetch(path, { headers: { "Content-Type": "application/json" }, ...opts });
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw Object.assign(new Error(body.error || r.statusText), { body });
    return body;
  };
  const fmtDur = s => s == null ? "" : `${Math.floor(s / 3600)} h ${String(Math.floor(s % 3600 / 60)).padStart(2, "0")}`;
  const fmtDate = iso => new Date(iso + "T12:00:00").toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short", year: "numeric" });
  const active = r => r.status === "queued" || r.status === "running";

  // ------------------------------------------------------------------ boot
  const [meta, cantons] = await Promise.all([api("/api/meta"), api("/api/cantons")]);
  const themeBtn = $("#theme");
  const view = new RouteView($("#map"), cantons, {
    onTheme: dark => { themeBtn.textContent = dark ? "Light" : "Dark"; },
    onSelect: i => setHash(state.selected, i),
  });
  themeBtn.addEventListener("click", () => view.toggleTheme());
  $("#all").addEventListener("click", () => view.select(-1));

  const state = { routes: [], selected: null, detail: null, renderedId: null, sort: "new" };

  // ------------------------------------------------------------------ form
  const dateIn = $("#date");
  if (meta.feed) { dateIn.min = meta.feed[0]; dateIn.max = meta.feed[1]; }
  dateIn.value = meta.default_date;
  $("#quality").innerHTML = meta.qualities.map((q, i) =>
    `<label><input type="radio" name="quality" value="${q.key}" ${i === 0 ? "checked" : ""}>${esc(q.label)}</label>`).join("");
  const qHint = () => {
    const q = meta.qualities.find(q => q.key === $("input[name=quality]:checked").value);
    $("#quality-hint").textContent = q.hint;
  };
  $("#quality").addEventListener("change", qHint);
  qHint();

  // station autocomplete (used by the start field and the via field)
  const highlight = (name, q) => {
    const i = name.toLowerCase().indexOf(q.toLowerCase());
    return i < 0 ? esc(name) : esc(name.slice(0, i)) + "<mark>" + esc(name.slice(i, i + q.length)) + "</mark>" + esc(name.slice(i + q.length));
  };
  function autocomplete(input, list, onPick) {
    let items = [], idx = -1, timer = null, seq = 0;
    const close = () => { list.hidden = true; input.setAttribute("aria-expanded", "false"); idx = -1; };
    const show = () => {
      if (!items.length) return close();
      list.innerHTML = items.map((s, i) =>
        `<li role="option" id="${list.id}-${i}" data-i="${i}" aria-selected="${i === idx}">
           <span>${highlight(s.name, input.value.trim())}</span><span class="c">${esc(s.canton)}</span></li>`).join("");
      list.hidden = false;
      input.setAttribute("aria-expanded", "true");
      if (idx >= 0) input.setAttribute("aria-activedescendant", `${list.id}-${idx}`);
    };
    const pick = i => { onPick(items[i].name); close(); };
    input.addEventListener("input", () => {
      clearTimeout(timer);
      const q = input.value.trim();
      if (!q) { items = []; return close(); }
      timer = setTimeout(async () => {
        const n = ++seq;
        const res = await api("/api/stations?q=" + encodeURIComponent(q));
        if (n !== seq) return;
        items = res; idx = res.length ? 0 : -1; show();
      }, 120);
    });
    input.addEventListener("keydown", e => {
      if (list.hidden) return;
      if (e.key === "ArrowDown") { e.preventDefault(); idx = (idx + 1) % items.length; show(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); idx = (idx - 1 + items.length) % items.length; show(); }
      else if (e.key === "Enter" && idx >= 0) { e.preventDefault(); pick(idx); }
      else if (e.key === "Escape") { e.stopPropagation(); close(); }
    });
    list.addEventListener("mousedown", e => { const li = e.target.closest("li"); if (li) { e.preventDefault(); pick(+li.dataset.i); } });
    input.addEventListener("blur", () => setTimeout(close, 100));
  }
  const st = $("#station");
  autocomplete(st, $("#suggest"), name => { st.value = name; });
  const endIn = $("#end_station");
  autocomplete(endIn, $("#end-suggest"), name => { endIn.value = name; });

  // via stations: ordered chips
  let via = [];
  const viaIn = $("#via-in");
  function renderVia() {
    $("#via-list").innerHTML = via.map((v, i) =>
      `<li><span class="num">${i + 1}</span>${esc(v)}
         <button type="button" class="x" data-i="${i}" aria-label="Remove ${esc(v)}">×</button></li>`).join("");
    viaIn.disabled = via.length >= 5;
    $("#via_first").closest(".toggle").hidden = via.length === 0;
    viaIn.placeholder = via.length >= 5 ? "max. 5 stations" : via.length ? "then…" : "e.g. Brig, Lugano, Chur";
  }
  autocomplete(viaIn, $("#via-suggest"), name => { if (via.length < 5) via.push(name); viaIn.value = ""; renderVia(); });
  $("#via-list").addEventListener("click", e => {
    const b = e.target.closest(".x");
    if (b) { via.splice(+b.dataset.i, 1); renderVia(); }
  });
  renderVia();

  // "already validated" canton picker (codes in KANTONSNUMMER order; names from the map data)
  const cantonName = Object.fromEntries(cantons.features.map(f => [f.properties.canton, f.properties.name]));
  const haveBox = $("#have");
  haveBox.innerHTML = meta.canton_codes.map((code, i) =>
    `<label title="${esc(cantonName[i] || code)}"><input type="checkbox" value="${code}"><span>${code}</span></label>`).join("")
    + `<button type="button" class="link" id="have-clear">clear</button>`;
  const getHave = () => [...haveBox.querySelectorAll("input:checked")].map(i => i.value);
  const setHave = codes => haveBox.querySelectorAll("input").forEach(i => { i.checked = codes.includes(i.value); });
  $("#have-clear").addEventListener("click", () => setHave([]));

  // detour rules description
  api("/api/detours").then(rows => {
    const by = {};
    rows.forEach(r => { (by[r.cantons] = by[r.cantons] || []).push(r.station); });
    $("#detour-desc").textContent = rows.length
      ? Object.entries(by).map(([c, st]) => `at ${st.join(" / ")} without ${c} → go there first`).join(" · ")
      : "none defined in detour_rules.tsv";
  }).catch(() => {});

  // bonus stations description
  // run-for-it stations + finishing runs (main box), tight tricks (own box)
  api("/api/bonus").then(rows => {
    const fmtC = r => r.cantons.split(",").map(x => x.trim()).join(" + ");
    const desc = r => r.excursion
      ? `${r.station} → bike via ${fmtC(r)} → ${r.to} (${r.minutes} min)`
      : `${r.station} (≥ ${r.minutes} min) → ${fmtC(r)}`;
    const tight = rows.filter(r => r.tight);
    $("#tight-desc").textContent = tight.length ? tight.map(desc).join(" · ") : "none marked \"tight\"";
    rows = rows.filter(r => !r.tight);
    $("#bonus-desc").textContent = rows.length
      ? rows.filter(r => !r.finish).map(desc).join(" · ")
        + (rows.some(r => r.finish) ? " · to finish, as the last canton: "
          + rows.filter(r => r.finish).map(r => `${r.station} → ${r.cantons} (${r.minutes} min)`).join(" · ") : "")
      : "none defined in bonus_stations.tsv";
  }).catch(() => {});

  // cycling needs the walk/bike links: disable the bike box when walking is off
  $("#walk").addEventListener("change", e => { $("#bike").disabled = !e.target.checked; });

  const formErr = msg => { const el = $("#form-err"); el.hidden = !msg; el.textContent = msg || ""; };
  $("#new").addEventListener("submit", async e => {
    e.preventDefault();
    formErr("");
    const body = {
      station: st.value.trim(), date: dateIn.value, start: $("#start").value,
      sweep_end: $("#sweep_end").value || null, quality: $("input[name=quality]:checked").value,
      min_dwell: +$("#min_dwell").value || 0, walk: $("#walk").checked, bike: $("#bike").checked,
      via, via_first: $("#via_first").checked, end_station: endIn.value.trim() || null, bonus: $("#bonus").checked, tight: $("#tight").checked, detour: $("#detour").checked, ride: $("#ride").checked, zero_change: $("#zero_change").checked, have: getHave(),
      note: $("#note").value.trim(),
    };
    if (!body.station) return formErr("Enter a start station.");
    if (!body.date) return formErr("Pick a date.");
    const btn = $("#submit");
    btn.disabled = true;
    try {
      const { id } = await api("/api/routes", { method: "POST", body: JSON.stringify(body) });
      await refreshList();
      openRoute(id);
    } catch (err) {
      const s = err.body?.suggestions;
      formErr(err.message + (s?.length ? ` Did you mean: ${s.map(x => x.name).join(", ")}?` : ""));
    } finally {
      btn.disabled = false;
    }
  });

  // ------------------------------------------------------------------ list
  $("#sort").addEventListener("change", e => { state.sort = e.target.value; renderList(); });

  function cardHtml(r) {
    const span = r.sweep_end ? `${r.start}–${r.sweep_end}` : `from ${r.start}`;
    const q = meta.qualities.find(x => x.key === r.quality)?.label ?? r.quality;
    let body = "";
    if (r.status === "done") {
      body = `<div class="res"><span class="dur">${fmtDur(r.duration_s)}</span>
                <span class="span">${esc(r.first_departure)} → ${esc(r.arrival)}${r.complete ? "" : ` · ${r.cantons}/26 cantons`}</span></div>`;
    } else if (active(r)) {
      const pct = r.progress_total ? Math.round(100 * r.progress_done / r.progress_total) : null;
      body = `<div class="meta">${esc(r.message || "")}${pct != null ? ` · ${r.progress_done}/${r.progress_total}` : ""}</div>
              <div class="progress ${pct == null ? "indet" : ""}"><i style="width:${pct ?? 0}%"></i></div>`;
    } else {
      body = `<div class="err">${esc(r.message || r.status)}</div>`;
    }
    const label = { done: "done", running: "running", queued: "queued", failed: "failed", cancelled: "cancelled" }[r.status] || r.status;
    return `<li class="rcard ${state.selected === r.id ? "sel" : ""}" data-id="${r.id}" tabindex="0">
              <div class="st">${esc(r.station)}</div><span class="chip ${r.status}">${label}</span>
              <div class="meta">${esc(fmtDate(r.date))} · ${esc(span)} · ${esc(q)}${r.min_dwell ? ` · dwell ≥ ${r.min_dwell}s` : ""}${r.walk ? (r.bike ? " · 🚲" : "") : " · no walking"}</div>
              ${r.end_station ? `<div class="meta">must end at ${esc(r.end_station)}</div>` : ""}
              ${JSON.parse(r.have || "[]").length ? `<div class="meta">already: ${JSON.parse(r.have).join(", ")}</div>` : ""}
              ${JSON.parse(r.via || "[]").length ? `<div class="meta">${r.via_first ? "first to " : "via "}${JSON.parse(r.via).map(esc).join(" → ")}</div>` : ""}
              ${r.note ? `<div class="note">${esc(r.note)}</div>` : ""}
              ${body}</li>`;
  }

  function renderList() {
    const rs = [...state.routes];
    if (state.sort === "fast") rs.sort((a, b) => (b.complete ?? -1) - (a.complete ?? -1) || (a.duration_s ?? 1e9) - (b.duration_s ?? 1e9));
    $("#routes").innerHTML = rs.length ? rs.map(cardHtml).join("")
      : `<li class="empty">No routes yet. Enter a start station above.</li>`;
  }
  $("#routes").addEventListener("click", e => { const c = e.target.closest(".rcard"); if (c) openRoute(+c.dataset.id); });
  $("#routes").addEventListener("keydown", e => { const c = e.target.closest(".rcard"); if (c && e.key === "Enter") openRoute(+c.dataset.id); });

  async function refreshList() {
    state.routes = await api("/api/routes");
    renderList();
  }

  // ------------------------------------------------------------------ detail
  function setHash(id, leg) {
    const h = id == null ? "" : `#route=${id}` + (leg >= 0 ? `&leg=${leg}` : "");
    if (location.hash !== h) history.replaceState(null, "", h || location.pathname);
  }

  async function openRoute(id, leg = -1) {
    state.selected = id;
    $("#home").hidden = true;
    $("#detail").hidden = false;
    $("#side").scrollTop = 0;
    renderList();
    setHash(id, leg);
    await loadDetail(leg);
  }

  async function loadDetail(leg = -1) {
    const id = state.selected;
    let d;
    try { d = await api(`/api/routes/${id}`); } catch { return closeDetail(); }
    if (state.selected !== id) return;
    state.detail = d;
    const job = $("#job"), it = $("#itinerary");
    if (d.status === "done" && d.result) {
      job.hidden = true;
      if (state.renderedId !== id) {
        view.render(d.result, it, { leg });
        state.renderedId = id;
      }
      return;
    }
    // not done: show job status (and clear the map)
    if (state.renderedId !== null) { view.clear(); view.fitSwitzerland(); it.innerHTML = ""; state.renderedId = null; }
    const pct = d.progress_total ? Math.round(100 * d.progress_done / d.progress_total) : null;
    job.hidden = false;
    job.innerHTML = `
      <h1>${esc(d.station)}</h1>
      <div class="sub">${esc(fmtDate(d.date))} · ${d.sweep_end ? `${esc(d.start)}–${esc(d.sweep_end)}` : `from ${esc(d.start)}`}</div>
      <div class="msg"><span class="chip ${d.status}">${esc(d.status)}</span> ${esc(d.message || "")}
        ${pct != null ? ` · ${d.progress_done}/${d.progress_total} searches` : ""}
        ${d.started && active(d) ? ` · ${Math.round(Date.now() / 1000 - d.started)} s` : ""}</div>
      ${active(d) ? `<div class="progress ${pct == null ? "indet" : ""}"><i style="width:${pct ?? 0}%"></i></div>` : ""}
      ${d.log ? `<pre>${esc(d.log.split("\n").filter(l => !l.startsWith("progress")).slice(-60).join("\n"))}</pre>` : ""}`;
    const pre = job.querySelector("pre");
    if (pre) pre.scrollTop = pre.scrollHeight;
  }

  function closeDetail() {
    state.selected = null;
    state.renderedId = null;
    view.clear();
    view.fitSwitzerland();
    $("#itinerary").innerHTML = "";
    $("#detail").hidden = true;
    $("#home").hidden = false;
    setHash(null);
    renderList();
  }
  $("#back").addEventListener("click", closeDetail);
  $("#del").addEventListener("click", async () => {
    const d = state.detail;
    if (!d || !confirm(`Delete the route from ${d.station}${active(d) ? " (and stop the computation)" : ""}?`)) return;
    await api(`/api/routes/${d.id}`, { method: "DELETE" });
    closeDetail();
    refreshList();
  });
  $("#redo").addEventListener("click", () => {
    const d = state.detail;
    if (!d) return;
    st.value = d.station; dateIn.value = d.date; $("#start").value = d.start; $("#sweep_end").value = d.sweep_end || "";
    const q = document.querySelector(`input[name=quality][value="${d.quality}"]`); if (q) q.checked = true; qHint();
    $("#min_dwell").value = d.min_dwell; $("#walk").checked = !!d.walk; $("#bike").checked = !!d.bike;
    $("#bonus").checked = !!d.bonus;
    $("#tight").checked = !!d.tight;
    $("#detour").checked = !!d.detour;
    $("#ride").checked = !!d.ride;
    $("#zero_change").checked = !!d.zero_change;
    setHave(JSON.parse(d.have || "[]"));
    via = JSON.parse(d.via || "[]"); renderVia();
    $("#via_first").checked = !!d.via_first;
    endIn.value = d.end_station || "";
    $("#note").value = d.note || "";
    if (d.min_dwell || !d.walk || d.note) $(".adv").open = true;
    closeDetail();
    st.focus();
  });

  // ------------------------------------------------------------------ polling
  async function tick() {
    try {
      await refreshList();
      if (state.selected != null) {
        const r = state.routes.find(r => r.id === state.selected);
        if (!r) closeDetail();
        else if (active(r) || state.renderedId !== r.id) await loadDetail();
      }
    } catch (e) { /* server restarting: try again later */ }
    const busy = state.routes.some(active);
    setTimeout(tick, busy ? 1500 : 8000);
  }

  // deep link #route=ID&leg=N
  const m = location.hash.match(/route=(\d+)(?:&leg=(\d+))?/);
  await refreshList();
  if (m && state.routes.some(r => r.id === +m[1])) openRoute(+m[1], m[2] ? +m[2] : -1);
  tick();
})();
