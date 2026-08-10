// rpprobed browser UI.
//
// Deliberately dependency-free. Every byte is served by the daemon itself, so
// the UI works on an air-gapped host — which is the host you are most likely
// to be debugging. That rules out a CDN, and it also rules out vendoring a
// few hundred kilobytes of minified d3 to draw rectangles: the flame graph
// below is ~60 lines of DOM.
//
// Token handling: the daemon prints a Jupyter-style `?token=` URL. A query
// string ends up in shell history and any intermediary's logs, so the first
// thing this page does is move the token into sessionStorage and scrub it
// from the address bar. Every fetch then sends it as an Authorization header.

(function () {
  "use strict";

  const TOKEN_KEY = "probe_token";

  function captureToken() {
    const fromUrl = new URLSearchParams(window.location.search).get("token");
    if (fromUrl) {
      sessionStorage.setItem(TOKEN_KEY, fromUrl);
      // Drop the token from the visible URL without reloading.
      const clean = window.location.pathname + window.location.hash;
      window.history.replaceState({}, "", clean);
      return fromUrl;
    }
    return sessionStorage.getItem(TOKEN_KEY) || "";
  }

  const token = captureToken();
  const statusEl = document.getElementById("status");

  function setStatus(message, isError) {
    statusEl.textContent = message || "";
    statusEl.classList.toggle("error", Boolean(isError));
  }

  async function api(path, params) {
    const url = new URL(path, window.location.origin);
    Object.entries(params || {}).forEach(function (entry) {
      if (entry[1] !== "" && entry[1] !== false && entry[1] != null) {
        url.searchParams.set(entry[0], entry[1]);
      }
    });
    const response = await fetch(url, {
      headers: { Authorization: "Bearer " + token },
    });
    if (!response.ok) {
      let detail = response.statusText;
      try {
        const body = await response.json();
        detail = body.detail || body.error || detail;
      } catch (ignored) {
        // A non-JSON error body (401 from the middleware) is still an error;
        // the status text is a fine description of it.
      }
      throw new Error(response.status + ": " + detail);
    }
    return response.json();
  }

  // --- generic helpers ----------------------------------------------------

  function el(tag, attrs, children) {
    const node = document.createElement(tag);
    Object.entries(attrs || {}).forEach(function (entry) {
      if (entry[0] === "class") node.className = entry[1];
      else if (entry[0] === "style") node.setAttribute("style", entry[1]);
      else node.setAttribute(entry[0], entry[1]);
    });
    (children || []).forEach(function (child) {
      node.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
    });
    return node;
  }

  function fillRows(tableId, rows) {
    const body = document.querySelector("#" + tableId + " tbody");
    body.replaceChildren.apply(body, rows);
  }

  function formValues(formId) {
    const form = document.getElementById(formId);
    const values = {};
    new FormData(form).forEach(function (value, key) {
      values[key] = value === "on" ? true : value;
    });
    return values;
  }

  function whenText(ms) {
    if (!ms) return "";
    return new Date(Number(ms)).toISOString().replace("T", " ").replace(".000Z", "Z");
  }

  function bytesText(n) {
    const units = ["B", "KiB", "MiB", "GiB"];
    let value = Number(n) || 0;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024;
      unit += 1;
    }
    return (unit === 0 ? value : value.toFixed(1)) + " " + units[unit];
  }

  // --- views --------------------------------------------------------------

  async function loadProcesses() {
    const rows = await api("/v1/ps", formValues("ps-controls"));
    fillRows(
      "ps-table",
      rows.map(function (row) {
        const envPairs = Object.entries(row.env || {})
          .map(function (e) { return e[0] + "=" + e[1]; })
          .join(" ");
        const snapshot = el("button", { class: "row-action" }, ["Snapshot"]);
        snapshot.addEventListener("click", function () {
          takeSnapshot(row.pid);
        });
        return el("tr", {}, [
          el("td", { class: "mono" }, [String(row.pid)]),
          el("td", {}, [row.name || ""]),
          el("td", {}, [row.app_class || ""]),
          el("td", { class: "mono" }, [row.cwd || ""]),
          el("td", {}, [row.registered ? "yes" : "no"]),
          // Names only when no value was disclosed — the daemon decides which,
          // and the UI simply shows whichever it was given.
          el("td", { class: "mono" }, [envPairs || (row.env_names || []).join(" ")]),
          el("td", {}, [row.registered ? snapshot : ""]),
        ]);
      })
    );
    setStatus(rows.length + " process(es)");
  }

  async function takeSnapshot(pid) {
    setStatus("requesting snapshot of pid " + pid + "…");
    try {
      const reply = await api("/v1/snapshot", { pid: pid, start_time: 0 });
      setStatus("snapshot queued: job " + (reply.job_id || "(inline)"));
    } catch (error) {
      setStatus(String(error.message), true);
    }
  }

  async function loadCrashes() {
    const rows = await api("/v1/crashes", formValues("crash-controls"));
    fillRows(
      "crash-table",
      rows.map(function (row) {
        const download = el(
          "a",
          { class: "row-action", href: "/v1/artifacts/" + row.id + "?token=" + encodeURIComponent(token) },
          ["Download"]
        );
        return el("tr", {}, [
          el("td", { class: "mono" }, [whenText(row.crashed_at_ms)]),
          el("td", {}, [row.app_class || ""]),
          el("td", { class: "mono" }, [row.signature || ""]),
          el("td", {}, [row.fault_kind || ""]),
          el("td", { class: "mono" }, [String(row.pid)]),
          el("td", { class: "mono" }, [bytesText(row.artifact_bytes)]),
          el("td", {}, [download]),
        ]);
      })
    );
    setStatus(rows.length + " crash(es)");
  }

  async function loadStats() {
    const stats = await api("/v1/crashes/stats", formValues("stats-controls"));
    document.getElementById("stats-summary").textContent =
      stats.total +
      " crash(es) across " +
      stats.distinct_classes +
      " class(es), " +
      whenText(stats.first_unix_ms) +
      " → " +
      whenText(stats.last_unix_ms);
    fillRows(
      "stats-table",
      (stats.signatures || []).map(function (row) {
        return el("tr", {}, [
          el("td", { class: "mono" }, [row.signature]),
          el("td", { class: "mono" }, [String(row.count)]),
          el("td", { class: "mono" }, [whenText(row.first_unix_ms)]),
          el("td", { class: "mono" }, [whenText(row.last_unix_ms)]),
          el("td", {}, [(row.app_classes || []).join(", ")]),
        ]);
      })
    );
    setStatus("");
  }

  // --- flame graph --------------------------------------------------------

  // A hash-derived warm hue per frame name. Colour carries no meaning in a
  // flame graph beyond "these two boxes are the same function", so a stable
  // hash beats a palette: the same frame is the same colour across renders,
  // and there is no palette to run out of.
  function frameColor(name) {
    let hash = 0;
    for (let i = 0; i < name.length; i += 1) {
      hash = (hash * 31 + name.charCodeAt(i)) | 0;
    }
    const hue = 20 + (Math.abs(hash) % 40);
    const light = 55 + (Math.abs(hash >> 8) % 25);
    return "hsl(" + hue + ", 75%, " + light + "%)";
  }

  function renderFlame(root) {
    const canvas = document.getElementById("flame-canvas");
    canvas.replaceChildren();
    if (!root || !root.value) {
      canvas.appendChild(el("p", { class: "summary" }, ["No samples in this artifact."]));
      return;
    }

    // Breadth-first by depth so each level is one row, with each frame's
    // width proportional to its share of the *zoomed* root.
    let level = [{ node: root, offset: 0 }];
    let depth = 0;
    while (level.length) {
      const row = el("div", { class: "flame-row" });
      const next = [];
      let cursor = 0;
      level.forEach(function (entry) {
        const width = (entry.node.value / root.value) * 100;
        // Frames narrower than a pixel or two are unreadable and would just
        // pile up; folding them away keeps the row honest about what is
        // legible rather than drawing slivers nobody can click.
        if (width < 0.15) return;
        const box = el(
          "div",
          {
            class: "flame-frame",
            style:
              "width:" + width + "%;margin-left:" + (entry.offset - cursor) + "%;background:" + frameColor(entry.node.name),
            title: entry.node.name + " — " + entry.node.value + " samples",
          },
          [entry.node.name]
        );
        box.addEventListener("click", function () {
          renderFlame(entry.node === root ? window.__flameRoot : entry.node);
        });
        row.appendChild(box);
        cursor = entry.offset + width;

        let childOffset = entry.offset;
        (entry.node.children || []).forEach(function (child) {
          next.push({ node: child, offset: childOffset });
          childOffset += (child.value / root.value) * 100;
        });
      });
      canvas.appendChild(row);
      level = next;
      depth += 1;
      // A runaway depth means a recursive stack; stop drawing rather than
      // hang the tab building rows nobody will scroll to.
      if (depth > 200) break;
    }
  }

  async function loadFlame() {
    const values = formValues("flame-controls");
    if (!values.artifact) {
      setStatus("enter an artifact id", true);
      return;
    }
    const root = await api("/v1/flame", { artifact: values.artifact });
    window.__flameRoot = root;
    renderFlame(root);
    setStatus(root.value + " samples");
  }

  // --- wiring -------------------------------------------------------------

  const loaders = {
    processes: loadProcesses,
    crashes: loadCrashes,
    stats: loadStats,
    flame: loadFlame,
  };

  function show(view) {
    document.querySelectorAll("nav button").forEach(function (button) {
      button.classList.toggle("active", button.dataset.view === view);
    });
    document.querySelectorAll(".view").forEach(function (section) {
      section.classList.toggle("active", section.id === view);
    });
  }

  document.querySelectorAll("nav button").forEach(function (button) {
    button.addEventListener("click", function () {
      show(button.dataset.view);
      if (button.dataset.view !== "flame") {
        run(loaders[button.dataset.view]);
      }
    });
  });

  ["ps-controls", "crash-controls", "stats-controls", "flame-controls"].forEach(function (id) {
    document.getElementById(id).addEventListener("submit", function (event) {
      event.preventDefault();
      const view = document.getElementById(id).closest(".view").id;
      run(loaders[view]);
    });
  });

  function run(loader) {
    setStatus("loading…");
    loader().catch(function (error) {
      setStatus(String(error.message), true);
    });
  }

  if (!token) {
    setStatus("no token: open the URL rpprobed printed at startup", true);
  } else {
    run(loadProcesses);
  }
})();
