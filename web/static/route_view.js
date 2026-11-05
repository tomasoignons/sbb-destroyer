// Route viewer shared by the web app and the standalone map page.
//   const view = new RouteView(document.getElementById("map"), cantonsGeoJSON);
//   view.render(route, sidebarElement);   // route = solver --json output
//   view.clear();
(function () {
  const MODES = [
    { key: "rail",  label: "Train",     test: t => t >= 100 && t <= 117 },
    { key: "bus",   label: "Bus",       test: t => (t >= 700 && t <= 716) || (t >= 200 && t <= 209) },
    { key: "tram",  label: "Tram",      test: t => t >= 900 && t <= 906 },
    { key: "boat",  label: "Boat",      test: t => t >= 1000 && t <= 1021 },
    { key: "cable", label: "Cable car", test: t => t >= 1300 && t <= 1307 },
    { key: "funi",  label: "Funicular", test: t => t === 1400 },
    { key: "metro", label: "Metro",     test: t => t >= 400 && t <= 405 },
    { key: "bike",  label: "Bike",      test: () => false },
  ];
  const modeOf = leg => leg.kind === "bike" ? "bike" : leg.kind !== "ride" ? leg.kind
    : (MODES.find(m => m.test(leg.trip.route_type)) || MODES[0]).key;
  const css = name => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const colorOf = mode => mode === "walk" || mode === "run" ? css("--walk") : mode === "start" ? css("--text-primary") : css(`--m-${mode}`);
  const esc = s => String(s ?? "").replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  const ll = p => [p.lat, p.lon];
  const hasLL = p => p && (p.lat || p.lon);
  const TILE_URL = "https://wmts.geo.admin.ch/1.0.0/ch.swisstopo.pixelkarte-grau/default/current/3857/{z}/{x}/{y}.jpeg";
  const ATTRIBUTION = '&copy; <a href="https://www.swisstopo.admin.ch/">swisstopo</a> · timetable: SBB open data';

  function isDark() {
    const t = document.documentElement.dataset.theme;
    return t ? t === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
  }

  class RouteView {
    constructor(mapEl, cantons, opts = {}) {
      this.mapEl = mapEl;
      this.cantons = cantons;
      this.opts = opts;
      this.map = L.map(mapEl, { zoomControl: true }).setView([46.8, 8.2], 8);
      this.tileLayer = null;
      this.legs = [];
      this.visitByName = {};
      this.selected = -1;
      this.hovered = -1;
      this.routeGroup = L.featureGroup().addTo(this.map);
      this.pointGroup = L.layerGroup().addTo(this.map);
      this.legLayers = [];

      this.cantonLayer = L.geoJSON(cantons, {
        style: f => this.cantonStyle(f, false),
        onEachFeature: (f, layer) => {
          layer.bindTooltip(() => {
            if (this.pre?.has(f.properties.name)) {
              return `<b>${esc(f.properties.name)}</b><br><span class="muted">✓ already validated</span>`;
            }
            const v = this.visitByName[f.properties.name];
            return v
              ? `<b>${esc(f.properties.name)}</b><br><span class="muted">#${v.n} · ${esc(v.how)} at ${esc(v.stop)} · ${esc(v.time)}</span>`
              : `<b>${esc(f.properties.name)}</b>${this.legs.length ? '<br><span class="muted">not visited</span>' : ""}`;
          }, { className: "tt", sticky: true });
          layer.on("mouseover", () => layer.setStyle(this.cantonStyle(f, true)));
          layer.on("mouseout", () => layer.setStyle(this.cantonStyle(f, false)));
        },
      }).addTo(this.map);
      L.layerGroup(cantons.features.map(f => L.marker(f.properties.label, {
        icon: L.divIcon({ className: "clabel", html: `<div>${esc(f.properties.name)}</div>`, iconSize: [0, 0] }),
        interactive: false,
      }))).addTo(this.map);

      this.keyHandler = e => {
        if (!this.legs.length || /INPUT|SELECT|TEXTAREA/.test(document.activeElement?.tagName)) return;
        if (e.key === "ArrowRight" || e.key === "ArrowDown") { e.preventDefault(); this.select(Math.min(this.legs.length - 1, this.selected + 1), true); }
        if (e.key === "ArrowLeft" || e.key === "ArrowUp") { e.preventDefault(); this.select(Math.max(0, this.selected - 1), true); }
        if (e.key === "Escape") this.select(-1);
      };
      document.addEventListener("keydown", this.keyHandler);
      matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => this.applyTheme());
      this.applyTheme();
      this.fitSwitzerland();
    }

    cantonStyle(f, hover) {
      const visited = !!this.visitByName[f.properties.name] || !!this.pre?.has(f.properties.name);
      const anyRoute = this.legs.length > 0;
      return { color: css("--canton-line"), weight: hover ? 2 : 1, fillColor: css("--canton-fill"),
               fillOpacity: hover ? 0.22 : visited ? 0.10 : 0.03, dashArray: anyRoute && !visited ? "3 4" : null };
    }

    fitSwitzerland() { this.map.fitBounds(this.cantonLayer.getBounds(), { padding: [20, 20] }); }

    fitAll() {
      const b = this.routeGroup.getBounds();
      if (b.isValid()) this.map.fitBounds(b, { padding: [30, 30] }); else this.fitSwitzerland();
    }

    toggleTheme() {
      document.documentElement.dataset.theme = isDark() ? "light" : "dark";
      try { localStorage.setItem("sbb-theme", document.documentElement.dataset.theme); } catch (e) {}
      this.applyTheme();
    }

    applyTheme() {
      if (this.tileLayer) this.tileLayer.remove();
      this.tileLayer = L.tileLayer(TILE_URL, { attribution: ATTRIBUTION, maxZoom: 18, opacity: 0.55 }).addTo(this.map);
      this.tileLayer.bringToBack();
      this.mapEl.classList.toggle("dark", isDark());
      this.cantonLayer.eachLayer(l => l.setStyle(this.cantonStyle(l.feature, false)));
      if (this.side) {
        this.side.querySelectorAll(".leg").forEach((el, i) => el.style.setProperty("--c", colorOf(this.legs[i].mode)));
        this.renderLegend();
      }
      this.drawPoints();
      this.styleLegs();
      this.opts.onTheme?.(isDark());
    }

    clear() {
      this.routeGroup.clearLayers();
      this.pointGroup.clearLayers();
      this.legLayers.forEach(g => g.stops.remove());
      this.legLayers = [];
      this.legs = [];
      this.visitByName = {};
      this.visits = [];
      this.pre = new Set();
      this.selected = this.hovered = -1;
      this.side = null;
      this.cantonLayer.eachLayer(l => l.setStyle(this.cantonStyle(l.feature, false)));
    }

    // ------------------------------------------------------------------ render
    render(route, side, { leg = -1 } = {}) {
      this.clear();
      this.route = route;
      this.side = side;
      // hide platform changes inside a station (same name, nothing gained)
      this.legs = route.legs.map(l => ({ ...l, mode: modeOf(l) }))
        .filter(l => !(l.kind === "walk" && l.from.name === l.to.name && !l.new_cantons.length));
      this.visits = [];
      this.legs.forEach(l => l.new_cantons.forEach(g => this.visits.push({ ...g, leg: l })));
      this.pre = new Set(route.already_validated || []);
      const n0 = this.pre.size;  // numbering continues after the pre-validated ones
      this.visitByName = Object.fromEntries(this.visits.map((v, i) => [v.canton, { ...v, n: n0 + i + 1 }]));
      this.usedModes = [...new Set(this.legs.map(l => l.mode))];

      const rides = this.legs.filter(l => l.kind === "ride");
      const bikeKm = this.legs.filter(l => l.kind === "bike").reduce((a, l) => a + (l.km || 0), 0);
      const startName = this.legs[0]?.from.name ?? "?";
      const endName = this.legs[this.legs.length - 1]?.to.name ?? "?";
      const stats = [["Trip time", route.duration.replace("h", " h ")], ["First boarding", route.first_departure],
                     ["Last canton", route.arrival], ["Vehicles", rides.length]];
      side.innerHTML = `
        <div class="rv-header">
          <h1>${esc(startName)} → ${esc(endName)}</h1>
          <div class="sub">${esc(route.date)} · ${route.cantons_visited === 26 ? "all 26 cantons" : route.cantons_visited + " of 26 cantons"}${bikeKm ? ` · ${bikeKm.toFixed(1)} km by bike` : ""}</div>
          ${route.end ? `<div class="sub">■ must end at ${esc(route.end)}${route.complete ? "" : " <b>(not reached)</b>"}</div>` : ""}
          ${this.pre.size ? `<div class="sub">✓ ${this.pre.size} already validated: ${[...this.pre].map(esc).join(", ")}</div>` : ""}
          ${(route.via || []).length ? `<div class="sub">${route.via_first ? "first to" : "via"} ${route.via.map(esc).join(" → ")}${route.via_reached < route.via.length ? ` <b>(only ${route.via_reached} reached)</b>` : ""}</div>` : ""}
          <div class="stats">${stats.map(([k, v]) => `<div class="stat"><div class="v">${esc(v)}</div><div class="k">${k}</div></div>`).join("")}</div>
          ${route.complete ? "" : `<div class="warn">Incomplete: only ${route.cantons_visited} cantons reached.</div>`}
        </div>
        <div class="legend"></div>
        <div class="tabs" role="tablist">
          <button role="tab" aria-selected="true" data-tab="legs">Connections</button>
          <button role="tab" aria-selected="false" data-tab="cantons">Cantons</button>
        </div>
        <div class="hint">Click a connection to zoom · ← → step through · Esc shows everything</div>
        <ol class="legs"></ol>
        <table class="cantons" hidden></table>`;
      this.listEl = side.querySelector("ol.legs");
      this.ctabEl = side.querySelector("table.cantons");
      this.renderLegend();
      this.renderList();
      this.renderCantons();
      side.querySelectorAll(".tabs button").forEach(b => b.addEventListener("click", () => {
        side.querySelectorAll(".tabs button").forEach(x => x.setAttribute("aria-selected", x === b));
        this.listEl.hidden = b.dataset.tab !== "legs";
        this.ctabEl.hidden = b.dataset.tab !== "cantons";
      }));

      this.drawLegs();
      this.drawPoints();
      this.cantonLayer.eachLayer(l => l.setStyle(this.cantonStyle(l.feature, false)));
      this.styleLegs();
      if (leg >= 0 && leg < this.legs.length) this.select(leg, true); else this.fitAll();
    }

    renderLegend() {
      const el = this.side?.querySelector(".legend");
      if (!el) return;
      el.innerHTML = MODES.filter(m => this.usedModes.includes(m.key))
        .map(m => `<span><i class="sw" style="border-color:${colorOf(m.key)}"></i>${m.label}</span>`).join("")
        + (this.usedModes.includes("walk") ? `<span><i class="sw walk" style="border-color:${colorOf("walk")}"></i>Walk</span>` : "")
        + `<span><span class="num">n</span>canton n° in visit order</span>`;
    }

    renderList() {
      const gainHtml = g => {
        const v = this.visitByName[g.canton];
        return `<div class="gain"><span class="num">${v.n}</span><b>${esc(g.canton)}</b> — ${esc(g.how)} at ${esc(g.stop)} · ${esc(g.time)}</div>`;
      };
      const firstRide = this.legs.findIndex(l => l.kind === "ride" || l.kind === "bike");
      const nVia = (this.route.via || []).length;
      this.listEl.innerHTML = this.legs.map((l, i) => {
        const viaHtml = h => `<div class="gain via">→ <b>via ${h.index + 1}/${nVia}: ${esc(h.station)}</b> — ${esc(h.how)} at ${esc(h.stop)} · ${esc(h.time)}</div>`;
        const hits = l.via_hits || [];
        const endHtml = l.end_reached ? `<div class="gain via">■ <b>end: ${esc(this.route.end)}</b> reached · ${esc(l.arr)}</div>` : "";
        const gains = l.new_cantons.length || hits.length
          ? `<div class="gains">${l.new_cantons.map(gainHtml).join("")}${hits.map(viaHtml).join("")}${endHtml}</div>` : (endHtml ? `<div class="gains">${endHtml}</div>` : "");
        let what;
        if (l.kind === "start") {
          what = `<div class="what"><span class="badge">Start</span></div><div class="route">${esc(l.from.name)}</div>`;
        } else if (l.kind === "run") {
          const mins = Math.max(1, Math.round((l.arr_s - l.dep_s) / 60));
          what = `<div class="what"><span class="badge"><i></i>Run</span>
                    <span class="dir">leave ≥ ${mins} min after arriving</span></div>
                  <div class="route">${esc(l.from.name)}: across the border and back</div>`;
        } else if (l.kind === "walk" || l.kind === "bike") {
          const km = l.km != null ? ` · ${l.km.toLocaleString(undefined, { maximumFractionDigits: 1 })} km` : "";
          what = `<div class="what"><span class="badge"><i></i>${l.kind === "bike" ? "Bike" : "Walk"}${km}</span>
                    ${i === firstRide && l.kind === "bike" ? `<span class="dir">⏱ trip time starts</span>` : ""}</div>
                  <div class="route">${esc(l.from.name)}<span class="arr">→</span>${esc(l.to.name)}</div>`;
        } else {
          const t = l.trip;
          const extra = [t.number ? `n° ${t.number}` : "", t.agency].filter(Boolean).join(" · ");
          what = `<div class="what"><span class="badge"><i></i>${esc(t.vehicle)}</span>
                    <span class="dir">dir. ${esc(t.headsign)}${l.stayed_seated ? " · stay seated" : ""}</span></div>
                  <div class="route">${esc(l.from.name)}<span class="arr">→</span>${esc(l.to.name)}</div>
                  <div class="dir">${esc(extra)}${i === firstRide ? " · ⏱ trip time starts" : ""}</div>`;
        }
        const times = l.kind === "start" ? `<b>${esc(l.dep)}</b>` : `<b>${esc(l.dep)}</b>${esc(l.arr)}`;
        return `<li class="leg ${l.kind}" data-i="${i}" style="--c:${colorOf(l.mode)}">
                  <div class="t">${times}</div><div class="bar"></div><div>${what}${gains}</div></li>`;
      }).join("");
      this.listEl.addEventListener("click", e => {
        const li = e.target.closest(".leg");
        if (li) { const i = +li.dataset.i; this.select(this.selected === i ? -1 : i); }
      });
      this.listEl.addEventListener("mouseover", e => { const li = e.target.closest(".leg"); if (li) this.hover(+li.dataset.i, true); });
      this.listEl.addEventListener("mouseleave", () => this.hover(-1, false));
    }

    renderCantons() {
      const allNames = this.cantons.features.map(f => f.properties.name);
      const pre = [...this.pre];
      const order = [...pre, ...this.visits.map(v => v.canton),
                     ...allNames.filter(n => !this.visitByName[n] && !this.pre.has(n)).sort()];
      this.ctabEl.innerHTML = order.map(n => {
        const v = this.visitByName[n];
        if (this.pre.has(n)) {
          return `<tr data-c="${esc(n)}"><td><span class="num">✓</span>${esc(n)}</td><td class="how">already validated</td><td class="tm">—</td></tr>`;
        }
        return v
          ? `<tr data-c="${esc(n)}"><td><span class="num">${v.n}</span>${esc(n)}</td><td class="how">${esc(v.how)} · ${esc(v.stop)}</td><td class="tm">${esc(v.time)}</td></tr>`
          : `<tr class="missing" data-c="${esc(n)}"><td>${esc(n)}</td><td class="how">not visited</td><td class="tm">—</td></tr>`;
      }).join("");
      this.ctabEl.addEventListener("click", e => {
        const tr = e.target.closest("tr");
        if (!tr) return;
        const v = this.visitByName[tr.dataset.c];
        if (v) this.select(this.legs.indexOf(v.leg));
        else {
          const f = this.cantons.features.find(f => f.properties.name === tr.dataset.c);
          if (f) this.map.fitBounds(L.geoJSON(f).getBounds(), { padding: [40, 40] });
        }
      });
    }

    // ------------------------------------------------------------------ map layers
    drawLegs() {
      this.legLayers = this.legs.map((l, i) => {
        const pts = (l.path || []).filter(hasLL).map(ll);
        const g = { pts, stops: L.layerGroup() };
        if (pts.length < 2) return g;
        g.casing = L.polyline(pts, { interactive: false }).addTo(this.routeGroup);
        g.line = L.polyline(pts, { interactive: false }).addTo(this.routeGroup);
        g.hit = L.polyline(pts, { opacity: 0, weight: 16 }).addTo(this.routeGroup); // generous hit target
        const tip = l.kind === "walk" || l.kind === "bike"
          ? `<b>${l.kind === "bike" ? "Bike" : "Walk"}</b>${l.km != null ? ` ${l.km} km` : ""} · ${esc(l.dep)} → ${esc(l.arr)}<br>${esc(l.from.name)} → ${esc(l.to.name)}`
          : `<b>${esc(l.trip.vehicle)}</b> <span class="muted">dir. ${esc(l.trip.headsign)}</span><br>${esc(l.dep)} ${esc(l.from.name)}<br>${esc(l.arr)} ${esc(l.to.name)}`;
        g.hit.bindTooltip(tip, { className: "tt", sticky: true });
        g.hit.on("mouseover", () => this.hover(i, true));
        g.hit.on("mouseout", () => this.hover(-1, false));
        g.hit.on("click", () => this.select(i, true));
        (l.path || []).slice(1, -1).filter(hasLL).forEach(p => {
          L.circleMarker(ll(p), { radius: 3, weight: 1.5 })
            .bindTooltip(`${esc(p.name)} · ${esc(p.time)}`, { className: "tt", direction: "top" })
            .addTo(g.stops);
        });
        return g;
      });
    }

    drawPoints() {
      this.pointGroup.clearLayers();
      if (!this.legs.length) return;
      const seen = new Set();
      this.legs.forEach(l => [l.from, l.to].forEach(p => {
        if (!hasLL(p) || seen.has(p.name)) return;
        seen.add(p.name);
        L.circleMarker(ll(p), { radius: 4, color: css("--text-primary"), weight: 2, fillColor: css("--surface-1"), fillOpacity: 1 })
          .bindTooltip(`<b>${esc(p.name)}</b><br><span class="muted">${esc(p.canton)}</span>`, { className: "tt", direction: "top" })
          .addTo(this.pointGroup);
      }));
      this.visits.forEach((v, k) => {
        const p = (v.leg.path || []).find(q => q.name === v.stop && q.canton === v.canton) || v.leg.to;
        if (!hasLL(p)) return;
        L.marker(ll(p), { icon: L.divIcon({ className: "clabel", html: `<div><span class="num">${this.pre.size + k + 1}</span></div>`, iconSize: [0, 0] }) })
          .bindTooltip(`<b>#${this.pre.size + k + 1} ${esc(v.canton)}</b><br><span class="muted">${esc(v.how)} at ${esc(v.stop)} · ${esc(v.time)}</span>`,
                       { className: "tt", direction: "top", offset: [0, -8] })
          .on("click", () => this.select(this.legs.indexOf(v.leg), true))
          .addTo(this.pointGroup);
      });
    }

    styleLegs() {
      const surf = css("--surface-1");
      this.legLayers.forEach((g, i) => {
        if (!g.line) return;
        const l = this.legs[i];
        const focus = this.selected === i || this.hovered === i;
        const dim = (this.selected >= 0 || this.hovered >= 0) && !focus;
        const w = focus ? 6 : 3.5;
        g.casing.setStyle({ color: surf, weight: w + 4, opacity: dim ? 0.3 : 0.95, lineCap: "round", lineJoin: "round" });
        g.line.setStyle({ color: colorOf(l.mode), weight: w, opacity: dim ? 0.25 : 1, lineCap: "round", lineJoin: "round",
                          dashArray: l.kind === "walk" ? "2 7" : null });
        if (focus) g.line.bringToFront();
        g.stops.eachLayer(m => m.setStyle({ color: colorOf(l.mode), fillColor: surf, fillOpacity: 1 }));
        if (this.selected === i) g.stops.addTo(this.map); else g.stops.remove();
      });
      this.side?.querySelectorAll(".leg").forEach((el, i) => el.classList.toggle("sel", i === this.selected));
    }

    hover(i, on) { this.hovered = on ? i : -1; this.styleLegs(); }

    select(i, scroll) {
      this.selected = i;
      this.styleLegs();
      this.opts.onSelect?.(i);
      if (i < 0) { this.fitAll(); return; }
      const g = this.legLayers[i];
      if (g.pts.length >= 2) this.map.fitBounds(L.latLngBounds(g.pts), { padding: [60, 60], maxZoom: 13 });
      else if (g.pts.length === 1) this.map.setView(g.pts[0], 13);
      const el = this.listEl?.children[i];
      if (el && scroll) el.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }

    invalidate() { this.map.invalidateSize(); }
  }

  try { const t = localStorage.getItem("sbb-theme"); if (t) document.documentElement.dataset.theme = t; } catch (e) {}
  window.RouteView = RouteView;
  window.RouteView.esc = esc;
  window.RouteView.isDark = isDark;
})();
