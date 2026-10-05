#!/usr/bin/env python3
"""Export a project's taskr cards (plans + tasks) into a single HTML page.

Queries the taskr MCP controller (JSON-RPC over streamable HTTP) for the
project's plans and tasks and writes ONE self-contained
`index.html` — a single-page site with a sidebar menu (plans -> tasks),
in-page anchor links, status badges, a live filter box, and each card's
full detail (objective, outcome, notes, paths, gates, edges, raw JSON).
No external assets; opens offline. Read-only against taskr.

Usage:
  python3 export-project-tasks.py <project-slug-or-id> [--out DIR] [--url URL]

<project-slug-or-id> is required (taskr resolves a globally-unique slug or
a project UUID). Default out=/tmp/taskr-export/<project>, url=
http://127.0.0.1:3000/mcp.
"""
from __future__ import annotations

import argparse
import html
import json
import os
import re
import sys
import urllib.request

DEFAULT_URL = "http://127.0.0.1:3000/mcp"

STATUS_COLORS = {
    "Passed": "#1a7f37", "Delivered": "#1a7f37", "Running": "#0969da",
    "WaitingForValidation": "#9a6700", "Planned": "#6639ba",
    "Backlog": "#57606a", "Blocked": "#cf222e", "Failed": "#cf222e",
    "Canceled": "#8250df",
}


def _post(url, payload, session_id=None):
    headers = {"Content-Type": "application/json",
               "Accept": "application/json, text/event-stream"}
    if session_id:
        headers["mcp-session-id"] = session_id
    req = urllib.request.Request(url, json.dumps(payload).encode(), headers)
    with urllib.request.urlopen(req, timeout=120) as resp:
        sid = resp.headers.get("mcp-session-id")
        body = resp.read().decode()
        ctype = resp.headers.get("Content-Type", "")
    if "text/event-stream" in ctype:
        msgs = [json.loads(l[5:].strip()) for l in body.splitlines()
                if l.startswith("data:") and l[5:].strip()]
        return (msgs[-1] if msgs else None), sid
    return (json.loads(body) if body.strip() else None), sid


def call(url, tool, args, soft=False):
    _, sid = _post(url, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                              "clientInfo": {"name": "card-export", "version": "1"}}})
    try:
        _post(url, {"jsonrpc": "2.0", "method": "notifications/initialized"}, sid)
    except Exception:
        pass
    msg, _ = _post(url, {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}}, sid)
    if not msg or "error" in (msg or {}):
        if soft:
            return None
        raise SystemExit(f"taskr {tool} error: {json.dumps(msg)[:400]}")
    blob = "\n".join(c.get("text", "") for c in msg["result"].get("content", []))
    try:
        return json.loads(blob)
    except Exception:
        return blob


# Body fields authored via task_create/task_update that summaries omit.
# task_get nests the scope paths under a `scope` object; fetch_body flattens
# scope.{include_paths,exclude_paths,notes} up to the top level for rendering.
BODY_FIELDS = ("objective", "gates", "scope", "include_paths", "exclude_paths",
               "notes", "run_spec")


def fetch_body(url, tid):
    """Fetch a task's authored body via task_get (taskr). Returns a dict of
    BODY_FIELDS, or {} if task_get is unavailable/empty (graceful: the
    exporter still works before task_get ships, just summary-only)."""
    res = call(url, "task_get", {"task_id": tid}, soft=True)
    if not res:
        return {}
    # tolerate {task:{...}} | {...} | [{...}]
    rec = res
    if isinstance(rec, dict) and "task" in rec and isinstance(rec["task"], dict):
        rec = rec["task"]
    if isinstance(rec, list):
        rec = next((x for x in rec if isinstance(x, dict) and x.get("id") == tid),
                   rec[0] if rec else {})
    if not isinstance(rec, dict):
        return {}
    body = {k: rec[k] for k in BODY_FIELDS if rec.get(k) not in (None, "", [], {})}
    # flatten scope.{include_paths,exclude_paths,notes} to top level
    scope = body.pop("scope", None)
    if isinstance(scope, dict):
        for k in ("include_paths", "exclude_paths", "notes"):
            if scope.get(k) not in (None, "", [], {}) and k not in body:
                body[k] = scope[k]
    return body


def fetch_plan_body(url, pid):
    """Fetch a plan's authored brief via plan_get (taskr). Returns a dict with
    `brief` (and `outcome` if present), or {} if plan_get is unavailable
    (graceful: the exporter still works before plan_get ships — plans then
    render summary-only, i.e. outcome but no brief)."""
    res = call(url, "plan_get", {"plan_id": pid}, soft=True)
    if not res:
        return {}
    rec = res
    if isinstance(rec, dict) and "plan" in rec and isinstance(rec["plan"], dict):
        rec = rec["plan"]
    if isinstance(rec, list):
        rec = next((x for x in rec if isinstance(x, dict) and x.get("id") == pid),
                   rec[0] if rec else {})
    if not isinstance(rec, dict):
        return {}
    return {k: rec[k] for k in ("brief", "outcome") if rec.get(k) not in (None, "")}


def collect(obj, tasks, plans):
    if isinstance(obj, dict):
        i = obj.get("id", "")
        if isinstance(i, str) and i.startswith("task-") and "title" in obj:
            tasks[i] = obj
        elif isinstance(i, str) and i.startswith("plan-") and "title" in obj:
            plans[i] = obj
        for v in obj.values():
            collect(v, tasks, plans)
    elif isinstance(obj, list):
        for v in obj:
            collect(v, tasks, plans)


def task_num(tid):
    m = re.search(r"(\d+)", tid or "")
    return int(m.group(1)) if m else 0


def e(x):
    return html.escape("" if x is None else str(x))


def badge(status):
    return (f'<span class="badge" style="background:{STATUS_COLORS.get(status, "#57606a")}">'
            f'{e(status)}</span>')


def card_html(t):
    tid = t.get("id")
    parts = [f'<section class="card" id="{e(tid)}" data-search="'
             f'{e((tid + " " + (t.get("title") or "") + " " + (t.get("slug") or "")).lower())}">']
    parts.append(f'<h3>{e(tid)} — {e(t.get("title", ""))} {badge(t.get("status"))}</h3>')
    meta = " · ".join(f"{k}: {e(t.get(k))}" for k in ("plan_id", "slug", "status")
                      if t.get(k) is not None)
    parts.append(f'<p class="meta">{meta}</p>')
    for label, key in (("Objective", "objective"), ("Outcome", "outcome"),
                       ("Notes", "notes")):
        v = t.get(key)
        if v:
            parts.append(f'<h4>{label}</h4><div class="prose">{e(v)}</div>')
    for key, label in (("include_paths", "Include paths"), ("exclude_paths", "Exclude paths"),
                       ("gates", "Gates"), ("blockers", "Blockers")):
        v = t.get(key)
        if v:
            items = v if isinstance(v, list) else [v]
            parts.append(f'<h4>{label}</h4><ul class="paths">'
                         + "".join(f"<li><code>{e(x)}</code></li>" for x in items) + "</ul>")
    for key, label in (("edges", "Edges"), ("run_spec", "Run spec")):
        v = t.get(key)
        if v:
            parts.append(f'<h4>{label}</h4><pre>{e(json.dumps(v, indent=2))}</pre>')
    parts.append('<details><summary>Raw JSON</summary>'
                 f'<pre>{e(json.dumps(t, indent=2, sort_keys=True))}</pre></details>')
    parts.append('<a class="top" href="#top">↑ top</a></section>')
    return "\n".join(parts)


def build_html(project, plans, tasks):
    by_plan = {}
    for tid, t in tasks.items():
        by_plan.setdefault(t.get("plan_id") or "no-plan", []).append(tid)
    plan_order = sorted(plans, key=lambda p: task_num(p))
    if any((t.get("plan_id") or "no-plan") not in plans for t in tasks.values()):
        plan_order.append("no-plan")

    nav, body = [], []
    for pid in plan_order:
        title = plans.get(pid, {}).get("title", "(no plan)") if pid != "no-plan" else "Unassigned"
        pstatus = plans.get(pid, {}).get("status", "")
        members = sorted(by_plan.get(pid, []), key=task_num)
        nav.append(f'<li class="plan"><a href="#{e(pid)}"><b>{e(pid)}</b> '
                   f'{badge(pstatus) if pstatus else ""}<br><span>{e(title)}</span></a><ul>')
        for tid in members:
            t = tasks[tid]
            nav.append(f'<li class="nav-task" data-search="'
                       f'{e((tid + " " + (t.get("title") or "")).lower())}">'
                       f'<a href="#{e(tid)}">{e(tid)} {badge(t.get("status"))}'
                       f'<span>{e(t.get("title",""))}</span></a></li>')
        nav.append("</ul></li>")

        body.append(f'<section class="plan-block" id="{e(pid)}"><h2>{e(pid)} — {e(title)} '
                    f'{badge(pstatus) if pstatus else ""}</h2>')
        p = plans.get(pid)
        if p and (p.get("brief") or p.get("outcome")):
            body.append(f'<div class="prose">{e(p.get("brief") or p.get("outcome"))}</div>')
        for tid in members:
            body.append(card_html(tasks[tid]))
        body.append("</section>")

    counts = {}
    for t in tasks.values():
        counts[t.get("status")] = counts.get(t.get("status"), 0) + 1
    summary = " · ".join(f"{badge(s)} {n}" for s, n in sorted(counts.items()))

    return f"""<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>taskr cards — {e(project)}</title>
<style>
:root{{color-scheme:light dark}}
*{{box-sizing:border-box}}
body{{margin:0;font:14px/1.5 system-ui,sans-serif;display:flex}}
#nav{{width:340px;min-width:340px;height:100vh;overflow:auto;border-right:1px solid #8884;padding:12px;position:sticky;top:0}}
#nav h1{{font-size:15px;margin:.2em 0}}
#nav ul{{list-style:none;margin:0;padding:0 0 0 6px}}
#nav li.plan>a{{display:block;padding:6px 4px;text-decoration:none;color:inherit;border-top:1px solid #8883;margin-top:6px}}
#nav li.plan>a span{{color:#8a8a8a;font-size:12px}}
.nav-task a{{display:block;padding:2px 4px;text-decoration:none;color:inherit;font-size:12px}}
.nav-task a span{{display:block;color:#8a8a8a;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.nav-task a:hover,#nav li.plan>a:hover{{background:#8882;border-radius:4px}}
#main{{flex:1;height:100vh;overflow:auto;padding:16px 28px;max-width:1000px}}
.plan-block{{margin-bottom:28px}}
.card{{border:1px solid #8884;border-radius:8px;padding:12px 16px;margin:12px 0;background:#8881}}
.card h3{{margin:.1em 0}}
.meta{{color:#8a8a8a;font-size:12px;margin:.2em 0 .6em}}
.prose{{white-space:pre-wrap}}
h4{{margin:.8em 0 .2em;font-size:13px;text-transform:uppercase;letter-spacing:.04em;color:#8a8a8a}}
ul.paths{{margin:.2em 0}}
code{{background:#8882;padding:1px 4px;border-radius:3px}}
pre{{background:#0001;padding:10px;border-radius:6px;overflow:auto;font-size:12px}}
.badge{{color:#fff;padding:1px 7px;border-radius:10px;font-size:11px;vertical-align:middle}}
.top{{font-size:11px;color:#8a8a8a;text-decoration:none}}
#filter{{width:100%;padding:6px 8px;margin:6px 0;border:1px solid #8886;border-radius:6px;background:transparent;color:inherit}}
.hidden{{display:none!important}}
</style></head><body>
<aside id="nav">
<h1>{e(project)}</h1>
<div>{len(plans)} plans · {len(tasks)} tasks</div>
<input id="filter" placeholder="filter tasks…" oninput="flt(this.value)">
<ul>{''.join(nav)}</ul>
</aside>
<main id="main"><a id="top"></a>
<h1>taskr cards — {e(project)}</h1>
<p>{summary}</p>
{''.join(body)}
</main>
<script>
function flt(q){{q=q.trim().toLowerCase();
document.querySelectorAll('.card,.nav-task').forEach(el=>{{
 const s=el.getAttribute('data-search')||'';
 el.classList.toggle('hidden', q!=='' && !s.includes(q));}});}}
</script></body></html>"""


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("project", help="project slug or UUID (taskr resolves either)")
    ap.add_argument("--out", default=None, help="output dir (default /tmp/taskr-export/<project>)")
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument("--no-bodies", action="store_true",
                    help="skip per-task task_get body fetch (summary-only)")
    args = ap.parse_args()

    out = args.out or os.path.join("/tmp/taskr-export",
                                   re.sub(r"[^A-Za-z0-9._-]+", "-", args.project))
    status = call(args.url, "orchestration_status",
                  {"project_id": args.project, "include_completed": True})
    tasks, plans = {}, {}
    collect(status, tasks, plans)
    if not tasks:
        print("No tasks found (check project slug/id and controller).", file=sys.stderr)
        sys.exit(1)

    # Enrich summaries with the authored body (objective/gates/scope/notes)
    # via task_get. orchestration_status is summary-only; task_get carries
    # the body. Graceful: if task_get is absent, bodies stay empty.
    bodies = plan_bodies = 0
    if not args.no_bodies:
        for tid in sorted(tasks):
            body = fetch_body(args.url, tid)
            if body:
                tasks[tid].update(body)
                bodies += 1
        if bodies == 0:
            print("note: no task bodies fetched — is task_get available on the "
                  "controller? (exporting summary-only)", file=sys.stderr)
        # Enrich plan summaries with the authored brief via plan_get; plan_list/
        # orchestration_status are summary-only (no brief). Graceful if absent.
        for pid in sorted(plans):
            pbody = fetch_plan_body(args.url, pid)
            if pbody:
                plans[pid].update(pbody)
                if pbody.get("brief"):
                    plan_bodies += 1
        if plan_bodies == 0 and plans:
            print("note: no plan briefs fetched — is plan_get available on the "
                  "controller? (plans exporting summary-only)", file=sys.stderr)

    os.makedirs(out, exist_ok=True)
    page = os.path.join(out, "index.html")
    with open(page, "w") as f:
        f.write(build_html(args.project, plans, tasks))
    print(f"Exported {len(tasks)} tasks ({bodies} with bodies) across "
          f"{len(plans)} plans ({plan_bodies} with briefs) -> {page}")


if __name__ == "__main__":
    main()
