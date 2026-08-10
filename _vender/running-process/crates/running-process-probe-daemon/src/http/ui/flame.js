// Flame-graph renderer. Hand-written and dependency-free.
//
// A flame graph is nested rectangles with widths proportional to sample count.
// What a library would add here is zoom and a tooltip — both a few lines —
// against a few hundred kilobytes of vendored minified JS in a page whose
// entire security property is being auditably self-contained.
//
// The page's CSP is `default-src 'none'`, so any external reference added
// later fails visibly rather than working only where the author tested it.

(function () {
  "use strict";

  const container = document.getElementById("flame");
  const root = typeof PROFILE === "undefined" ? null : PROFILE;

  // Colour is per-frame-name, derived from a hash. In a flame graph colour
  // carries no meaning beyond "these two boxes are the same function", so a
  // stable hash beats a palette: the same frame is the same colour across
  // renders, and there is no palette to exhaust.
  function frameColor(name) {
    let hash = 0;
    for (let i = 0; i < name.length; i += 1) {
      hash = (hash * 31 + name.charCodeAt(i)) | 0;
    }
    const hue = 20 + (Math.abs(hash) % 40);
    const light = 55 + (Math.abs(hash >> 8) % 25);
    return "hsl(" + hue + ", 75%, " + light + "%)";
  }

  function render(node) {
    container.replaceChildren();
    if (!node || !node.value) {
      const empty = document.createElement("p");
      empty.className = "empty";
      empty.textContent = "No samples in this profile.";
      container.appendChild(empty);
      return;
    }

    // Breadth-first by depth: one row per stack level, each frame's width a
    // share of the *zoomed* root rather than of the whole profile, so zooming
    // actually magnifies.
    let level = [{ node: node, offset: 0 }];
    let depth = 0;
    while (level.length) {
      const row = document.createElement("div");
      row.className = "flame-row";
      const next = [];
      let cursor = 0;

      level.forEach(function (entry) {
        const width = (entry.node.value / node.value) * 100;
        // Sub-pixel frames are unreadable and unclickable; folding them away
        // keeps the row honest about what is legible instead of drawing
        // slivers.
        if (width < 0.12) return;

        const box = document.createElement("div");
        box.className = "flame-frame";
        box.style.width = width + "%";
        box.style.marginLeft = entry.offset - cursor + "%";
        box.style.background = frameColor(entry.node.name);
        box.title = entry.node.name + " — " + entry.node.value + " samples";
        box.textContent = entry.node.name;
        box.addEventListener("click", function () {
          render(entry.node);
        });
        row.appendChild(box);
        cursor = entry.offset + width;

        let childOffset = entry.offset;
        (entry.node.children || []).forEach(function (child) {
          next.push({ node: child, offset: childOffset });
          childOffset += (child.value / node.value) * 100;
        });
      });

      container.appendChild(row);
      level = next;
      depth += 1;
      // Deep recursion would otherwise build rows until the tab stops
      // responding, and nobody scrolls to depth 200 anyway.
      if (depth > 200) break;
    }
  }

  document.querySelector("header").addEventListener("click", function () {
    render(root);
  });

  render(root);
})();
