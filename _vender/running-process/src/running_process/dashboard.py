"""Web-based dashboard for running-process daemon.

Launches a lightweight HTTP server and opens a browser to display a live
tree view of all tracked processes. Data is fetched from the
running-process-daemon via its ``list --json`` CLI command.

Usage::

    running-process-dashboard          # default port 8787
    running-process-dashboard --port 9000
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import threading
import webbrowser
from collections.abc import Sequence
from datetime import UTC, datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

_DEFAULT_PORT = 8787
_REFRESH_INTERVAL_MS = 3000
_STATE_NAMES = {1: "alive", 2: "dead", 3: "zombie"}


def _fetch_processes_json() -> list[dict]:
    """Query the daemon for the current process list (JSON)."""
    daemon_bin = shutil.which("running-process-daemon")
    if daemon_bin is None:
        return []
    try:
        result = subprocess.run(
            [daemon_bin, "list", "--json"],
            capture_output=True,
            text=True,
            timeout=5.0,
            check=False,
        )
        if result.returncode != 0:
            return []
        payload = json.loads(result.stdout)
        return payload if isinstance(payload, list) else []
    except (subprocess.TimeoutExpired, json.JSONDecodeError, OSError):
        return []


def _fetch_parent_pids(pids: Sequence[int]) -> dict[int, int | None]:
    if not pids:
        return {}
    try:
        if sys.platform == "win32":
            filter_expr = " OR ".join(f"ProcessId = {int(pid)}" for pid in pids)
            command = [
                "powershell",
                "-NoProfile",
                "-Command",
                (
                    f"$items = Get-CimInstance Win32_Process -Filter '{filter_expr}' "
                    "-ErrorAction SilentlyContinue; "
                    "foreach ($item in $items) { "
                    "'{0} {1}' -f $item.ProcessId, $item.ParentProcessId }"
                ),
            ]
        else:
            command = ["ps", "-o", "pid=,ppid=", "-p", ",".join(str(int(pid)) for pid in pids)]
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=4.0,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError):
        return {}
    if result.returncode != 0:
        return {}

    parent_by_pid: dict[int, int | None] = {}
    for raw_line in result.stdout.splitlines():
        parts = raw_line.strip().split()
        if len(parts) != 2 or not all(part.isdigit() for part in parts):
            continue
        parent_by_pid[int(parts[0])] = int(parts[1])
    return parent_by_pid


def _state_name(value: int | None) -> str:
    return _STATE_NAMES.get(int(value or 0), "unknown")


def _format_timestamp(value: float | int | None) -> str:
    if value is None:
        return "unknown"
    try:
        return datetime.fromtimestamp(float(value), tz=UTC).strftime("%Y-%m-%d %H:%M:%SZ")
    except (OSError, OverflowError, ValueError):
        return "unknown"


def _format_originator(originator: str) -> str:
    if not originator:
        return "unknown"
    tool, separator, parent_pid = originator.rpartition(":")
    if separator and tool and parent_pid.isdigit():
        return f"{tool} ({parent_pid})"
    return originator


def _normalize_processes(processes: list[dict]) -> list[dict]:
    parent_by_pid = _fetch_parent_pids([int(proc["pid"]) for proc in processes if "pid" in proc])
    tracked_pids = {int(proc["pid"]) for proc in processes if "pid" in proc}
    normalized: list[dict] = []
    for proc in processes:
        pid = int(proc["pid"])
        parent_pid = parent_by_pid.get(pid)
        originator = str(proc.get("originator") or "")
        if parent_pid in tracked_pids:
            spawned_by = f"tracked pid {parent_pid}"
        elif originator:
            spawned_by = _format_originator(originator)
        elif parent_pid:
            spawned_by = f"pid {parent_pid}"
        else:
            spawned_by = "unknown"
        normalized.append(
            {
                **proc,
                "pid": pid,
                "parent_pid": parent_pid,
                "state_name": _state_name(proc.get("state")),
                "created_at_display": _format_timestamp(proc.get("created_at")),
                "registered_at_display": _format_timestamp(proc.get("registered_at")),
                "spawned_by": spawned_by,
                "originator_display": _format_originator(originator),
            }
        )
    return normalized


def _sort_tree_nodes(nodes: list[dict]) -> list[dict]:
    nodes.sort(
        key=lambda item: (
            float(item.get("created_at") or item.get("registered_at") or 0.0),
            int(item["pid"]),
        )
    )
    for node in nodes:
        node["children"] = _sort_tree_nodes(node["children"])
    return nodes


def _build_process_tree(processes: list[dict]) -> list[dict]:
    nodes = {int(proc["pid"]): {**proc, "children": []} for proc in processes}
    roots: list[dict] = []
    for pid, node in nodes.items():
        parent_pid = node.get("parent_pid")
        if parent_pid in nodes and parent_pid != pid:
            nodes[parent_pid]["children"].append(node)
        else:
            roots.append(node)
    return _sort_tree_nodes(roots)


def _dashboard_payload() -> dict:
    processes = _normalize_processes(_fetch_processes_json())
    tree = _build_process_tree(processes)
    return {
        "generated_at": _format_timestamp(datetime.now(tz=UTC).timestamp()),
        "summary": {
            "tracked": len(processes),
            "roots": len(tree),
        },
        "processes": processes,
        "tree": tree,
    }


_HTML_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>running-process dashboard</title>
<style>
  :root {
    --bg: #081019;
    --fg: #f5f1e8;
    --muted: #98a5b3;
    --line: rgba(152, 165, 179, 0.22);
    --surface: rgba(17, 28, 40, 0.8);
    --surface-strong: rgba(21, 35, 50, 0.96);
    --accent: #f3a54a;
    --accent-soft: rgba(243, 165, 74, 0.18);
    --alive: #79d29a;
    --dead: #ff8f7f;
    --zombie: #f2d06b;
    --unknown: #b8c2cc;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    min-height: 100vh;
    font-family: "IBM Plex Sans", "Segoe UI", sans-serif;
    color: var(--fg);
    background:
      radial-gradient(circle at top left, rgba(243, 165, 74, 0.14), transparent 28rem),
      linear-gradient(180deg, #0a121c, #081019 48%, #060d15);
  }
  .shell {
    max-width: 1120px;
    margin: 0 auto;
    padding: 28px 20px 40px;
  }
  .hero {
    display: grid;
    gap: 16px;
    margin-bottom: 24px;
  }
  .title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
  }
  h1 {
    margin: 0;
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 1.65rem;
    font-weight: 600;
    letter-spacing: 0.01em;
  }
  .dot {
    width: 11px;
    height: 11px;
    border-radius: 999px;
    background: var(--alive);
    box-shadow: 0 0 0 6px rgba(121, 210, 154, 0.12);
  }
  .dot.offline {
    background: var(--dead);
    box-shadow: 0 0 0 6px rgba(255, 143, 127, 0.12);
  }
  .stamp {
    color: var(--muted);
    font-size: 0.95rem;
  }
  .summary-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 12px;
  }
  .summary-card {
    padding: 14px 16px;
    border: 1px solid var(--line);
    border-radius: 18px;
    background: linear-gradient(180deg, rgba(20, 31, 44, 0.88), rgba(13, 22, 33, 0.88));
    backdrop-filter: blur(8px);
  }
  .summary-label {
    color: var(--muted);
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    margin-bottom: 6px;
  }
  .summary-value {
    font-size: 1.7rem;
    font-weight: 600;
  }
  #error {
    display: none;
    padding: 12px 14px;
    border: 1px solid rgba(255, 143, 127, 0.35);
    border-radius: 14px;
    background: rgba(88, 24, 24, 0.35);
    color: var(--dead);
    margin-bottom: 18px;
  }
  .empty {
    display: none;
    padding: 48px 24px;
    border: 1px dashed var(--line);
    border-radius: 22px;
    text-align: center;
    color: var(--muted);
    background: rgba(10, 18, 28, 0.48);
  }
  .tree-root {
    display: grid;
    gap: 14px;
  }
  .node-children {
    margin-left: 22px;
    padding-left: 18px;
    border-left: 1px solid var(--line);
    display: grid;
    gap: 12px;
  }
  details.tree-node {
    border: 1px solid var(--line);
    border-radius: 20px;
    background: linear-gradient(180deg, var(--surface-strong), var(--surface));
    overflow: hidden;
  }
  details.tree-node[open] {
    box-shadow: 0 10px 24px rgba(0, 0, 0, 0.18);
  }
  summary.node-summary {
    list-style: none;
    cursor: pointer;
    padding: 16px 18px;
  }
  summary.node-summary::-webkit-details-marker {
    display: none;
  }
  .leaf-node {
    border: 1px solid var(--line);
    border-radius: 20px;
    background: linear-gradient(180deg, var(--surface-strong), var(--surface));
    padding: 16px 18px;
  }
  .node-head {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    align-items: start;
    flex-wrap: wrap;
  }
  .node-command {
    font-family: "IBM Plex Mono", "Cascadia Code", monospace;
    font-size: 0.96rem;
    line-height: 1.5;
    color: var(--fg);
    word-break: break-word;
  }
  .node-subtitle {
    margin-top: 6px;
    color: var(--muted);
    font-size: 0.88rem;
  }
  .node-badges {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    align-items: center;
  }
  .badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 5px 9px;
    border-radius: 999px;
    font-size: 0.78rem;
    font-weight: 600;
    letter-spacing: 0.03em;
    border: 1px solid var(--line);
    background: rgba(255, 255, 255, 0.03);
    color: var(--muted);
  }
  .badge.pid {
    color: var(--accent);
    border-color: rgba(243, 165, 74, 0.24);
    background: var(--accent-soft);
  }
  .badge.alive { color: var(--alive); }
  .badge.dead { color: var(--dead); }
  .badge.zombie { color: var(--zombie); }
  .badge.unknown { color: var(--unknown); }
  .meta-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 10px 14px;
    margin-top: 14px;
  }
  .meta-item {
    min-width: 0;
  }
  .meta-label {
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.72rem;
    margin-bottom: 4px;
  }
  .meta-value {
    font-size: 0.9rem;
    line-height: 1.35;
    word-break: break-word;
  }
  .tree-toggle {
    margin-right: 10px;
    color: var(--accent);
    font-family: "IBM Plex Mono", monospace;
  }
  @media (max-width: 720px) {
    .shell { padding-inline: 14px; }
    .node-children { margin-left: 10px; padding-left: 12px; }
    .summary-value { font-size: 1.35rem; }
  }
</style>
</head>
<body>
<div class="shell">
  <section class="hero">
    <div class="title-row">
      <h1><span class="dot" id="statusDot"></span> running-process dashboard</h1>
      <div class="stamp" id="generatedAt">loading...</div>
    </div>
    <div class="summary-grid">
      <div class="summary-card">
        <div class="summary-label">Tracked</div>
        <div class="summary-value" id="trackedCount">0</div>
      </div>
      <div class="summary-card">
        <div class="summary-label">Tree Roots</div>
        <div class="summary-value" id="rootCount">0</div>
      </div>
      <div class="summary-card">
        <div class="summary-label">Refresh</div>
        <div class="summary-value" id="refreshPeriod">3s</div>
      </div>
    </div>
  </section>

  <div id="error"></div>
  <div class="empty" id="empty">No tracked processes</div>
  <div class="tree-root" id="treeRoot"></div>
</div>

<script>
const REFRESH_MS = """ + str(_REFRESH_INTERVAL_MS) + r""";

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function renderMeta(node) {
  const items = [
    ['Created', node.created_at_display],
    ['Registered', node.registered_at_display],
    ['Spawned by', node.spawned_by],
    ['Originator', node.originator_display],
    ['Parent PID', node.parent_pid ?? 'none'],
    ['Working dir', node.cwd || 'unknown'],
  ];
  return items.map(([label, value]) =>
    '<div class="meta-item">' +
      '<div class="meta-label">' + escapeHtml(label) + '</div>' +
      '<div class="meta-value">' + escapeHtml(value) + '</div>' +
    '</div>'
  ).join('');
}

function renderNodeCard(node, expandable) {
  const toggle = expandable
    ? '<span class="tree-toggle">+</span>'
    : '<span class="tree-toggle">.</span>';
  return (
    '<div class="node-head">' +
      '<div>' +
        '<div class="node-command">' +
          toggle + escapeHtml(node.command || '(no command)') +
        '</div>' +
        '<div class="node-subtitle">' + escapeHtml(node.kind || 'unknown kind') + '</div>' +
      '</div>' +
      '<div class="node-badges">' +
        '<span class="badge pid">PID ' + escapeHtml(node.pid) + '</span>' +
        '<span class="badge ' + escapeHtml(node.state_name) + '">' +
          escapeHtml(node.state_name) +
        '</span>' +
      '</div>' +
    '</div>' +
    '<div class="meta-grid">' + renderMeta(node) + '</div>'
  );
}

function renderNode(node) {
  const children = Array.isArray(node.children) ? node.children : [];
  if (children.length === 0) {
    return '<div class="leaf-node">' + renderNodeCard(node, false) + '</div>';
  }
  return (
    '<details class="tree-node" open>' +
      '<summary class="node-summary">' + renderNodeCard(node, true) + '</summary>' +
      '<div class="node-children">' + children.map(renderNode).join('') + '</div>' +
    '</details>'
  );
}

async function refresh() {
  try {
    const resp = await fetch('/api/processes');
    if (!resp.ok) {
      throw new Error('HTTP ' + resp.status);
    }
    const data = await resp.json();
    const tree = data.tree || [];
    const summary = data.summary || {};

    document.getElementById('statusDot').className = 'dot';
    document.getElementById('generatedAt').textContent =
      'updated ' + (data.generated_at || 'unknown');
    document.getElementById('trackedCount').textContent = String(summary.tracked || 0);
    document.getElementById('rootCount').textContent = String(summary.roots || 0);
    document.getElementById('refreshPeriod').textContent = Math.floor(REFRESH_MS / 1000) + 's';
    document.getElementById('error').style.display = 'none';

    const empty = document.getElementById('empty');
    const treeRoot = document.getElementById('treeRoot');
    if (tree.length === 0) {
      treeRoot.innerHTML = '';
      empty.style.display = '';
      return;
    }

    empty.style.display = 'none';
    treeRoot.innerHTML = tree.map(renderNode).join('');
  } catch (error) {
    document.getElementById('statusDot').className = 'dot offline';
    const errorNode = document.getElementById('error');
    errorNode.textContent = 'Failed to fetch dashboard data: ' + error.message;
    errorNode.style.display = '';
  }
}

refresh();
setInterval(refresh, REFRESH_MS);
</script>
</body>
</html>"""


class _DashboardHandler(BaseHTTPRequestHandler):
    """Serve the dashboard HTML and JSON API."""

    def do_GET(self) -> None:
        if self.path in {"/", "/index.html"}:
            self._send_html()
        elif self.path == "/api/processes":
            self._send_json()
        else:
            self.send_error(404)

    def _send_html(self) -> None:
        body = _HTML_TEMPLATE.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self) -> None:
        payload = json.dumps(_dashboard_payload()).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Launch the running-process web dashboard")
    parser.add_argument(
        "--port",
        type=int,
        default=_DEFAULT_PORT,
        help=f"HTTP server port (default: {_DEFAULT_PORT})",
    )
    parser.add_argument(
        "--no-browser",
        action="store_true",
        help="Don't auto-open the browser",
    )
    args = parser.parse_args(argv)

    url = f"http://localhost:{args.port}"
    server = HTTPServer(("127.0.0.1", args.port), _DashboardHandler)

    if not args.no_browser:
        threading.Timer(0.5, webbrowser.open, args=(url,)).start()

    print(f"[dashboard] serving at {url}")
    print("[dashboard] press Ctrl+C to stop")
    try:
        server.serve_forever()
    except KeyboardInterrupt:  # noqa: KBI002
        # Top-level CLI handler on the main thread: Ctrl+C is the documented
        # way to stop the dashboard, so it exits cleanly rather than
        # propagating. Re-raising would replace an orderly shutdown with a
        # traceback for the expected way to quit.
        print("\n[dashboard] shutting down")
        server.shutdown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
