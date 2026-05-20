"""Self-contained HTML visualization using D3.js force-directed graph."""

from __future__ import annotations

import json
from typing import Any


# Color palette by language
_LANG_COLORS = {
    "python": "#3572A5",
    "hcl": "#844FBA",
    "yaml": "#CB171E",
    "jinja2": "#B41717",
}

# Color palette by node kind
_KIND_COLORS = {
    "Function": "#4CAF50",
    "Class": "#2196F3",
    "Method": "#8BC34A",
    "File": "#9E9E9E",
    "Resource": "#FF9800",
    "Variable": "#FFC107",
    "Module": "#00BCD4",
    "K8sResource": "#326CE5",
    "Document": "#795548",
    "Section": "#607D8B",
    "Template": "#E91E63",
    "DataSource": "#FF5722",
    "Output": "#CDDC39",
    "Provider": "#673AB7",
}

# Edge colors by kind
_EDGE_COLORS = {
    "CALLS": "#8CB4E0",
    "IMPORTS": "#A8D8A8",
    "INHERITS": "#E91E63",
    "REFERENCES": "#FF9800",
    "USES_VARIABLE": "#FFC107",
    "PASSES_VAR": "#CDDC39",
    "CONTAINS": "#555",
}


def render_graph_html(
    nodes: list[dict[str, Any]],
    edges: list[dict[str, Any]],
    title: str = "Code Rosetta Graph",
    color_by: str = "kind",
) -> str:
    """Render nodes and edges as a self-contained HTML file with D3.js."""

    # Build node id set for filtering dangling edges
    node_ids = {n["qualified_name"] for n in nodes}

    # Filter edges to only those connecting existing nodes
    valid_edges = [
        e for e in edges
        if e.get("source") in node_ids and e.get("target") in node_ids
    ]

    # Deduplicate edges
    seen_edges = set()
    deduped_edges = []
    for e in valid_edges:
        key = (e["source"], e["target"], e["kind"])
        if key not in seen_edges:
            seen_edges.add(key)
            deduped_edges.append(e)

    colors = _KIND_COLORS if color_by == "kind" else _LANG_COLORS

    # Build D3-compatible data
    d3_nodes = []
    for n in nodes:
        color_key = n.get("kind", "") if color_by == "kind" else n.get("language", "")
        d3_nodes.append({
            "id": n["qualified_name"],
            "name": n["name"],
            "kind": n.get("kind", ""),
            "language": n.get("language", ""),
            "file": n.get("file_path", ""),
            "line": n.get("line_start", 0),
            "color": colors.get(color_key, "#999"),
        })

    d3_edges = []
    for e in deduped_edges:
        d3_edges.append({
            "source": e["source"],
            "target": e["target"],
            "kind": e["kind"],
            "color": _EDGE_COLORS.get(e["kind"], "#ccc"),
        })

    graph_data = json.dumps({"nodes": d3_nodes, "links": d3_edges})

    return _HTML_TEMPLATE.replace("__GRAPH_DATA__", graph_data).replace("__TITLE__", title)


_HTML_TEMPLATE = """<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>__TITLE__</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: #1a1a2e; color: #eee; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; overflow: hidden; }
  #controls {
    position: fixed; top: 12px; left: 12px; z-index: 10;
    background: rgba(26, 26, 46, 0.9); padding: 12px 16px; border-radius: 8px;
    border: 1px solid #333; font-size: 13px; max-width: 320px;
  }
  #controls h2 { font-size: 14px; margin-bottom: 8px; color: #9b2335; }
  #controls .stat { color: #888; margin-bottom: 4px; }
  #search-box {
    width: 100%; padding: 6px 10px; margin: 8px 0; border-radius: 4px;
    border: 1px solid #444; background: #16213e; color: #eee; font-size: 13px;
  }
  #search-box:focus { outline: none; border-color: #9b2335; }
  #legend { margin-top: 10px; }
  #legend .item { display: inline-flex; align-items: center; margin: 2px 8px 2px 0; font-size: 12px; }
  #legend .dot { width: 10px; height: 10px; border-radius: 50%; margin-right: 4px; display: inline-block; }
  #tooltip {
    position: fixed; display: none; background: rgba(22, 33, 62, 0.95); color: #eee;
    padding: 10px 14px; border-radius: 6px; border: 1px solid #444;
    font-size: 12px; pointer-events: none; z-index: 20; max-width: 500px;
  }
  #tooltip .tt-name { font-weight: 600; font-size: 14px; color: #fff; }
  #tooltip .tt-kind { color: #9b2335; margin-left: 6px; }
  #tooltip .tt-file { color: #888; margin-top: 4px; word-break: break-all; }
  svg { width: 100vw; height: 100vh; }
  .link { stroke-opacity: 0.8; }
  .link:hover { stroke-opacity: 1; }
  .node circle { stroke: #fff; stroke-width: 1.5; cursor: pointer; }
  .node text { fill: #ccc; font-size: 11px; pointer-events: none; }
  .node.highlighted circle { stroke: #9b2335; stroke-width: 3; }
  .node.dimmed circle { opacity: 0.15; }
  .node.dimmed text { opacity: 0.1; }
  .link.dimmed { stroke-opacity: 0.05; }
</style>
</head>
<body>
<div id="controls">
  <h2>__TITLE__</h2>
  <div class="stat" id="stat-nodes"></div>
  <div class="stat" id="stat-edges"></div>
  <input type="text" id="search-box" placeholder="Filter nodes...">
  <div id="legend"></div>
</div>
<div id="tooltip">
  <span class="tt-name"></span><span class="tt-kind"></span>
  <div class="tt-file"></div>
</div>
<svg></svg>

<script src="https://d3js.org/d3.v7.min.js"></script>
<script>
const data = __GRAPH_DATA__;

document.getElementById('stat-nodes').textContent = `Nodes: ${data.nodes.length}`;
document.getElementById('stat-edges').textContent = `Edges: ${data.links.length}`;

// Build node kind legend
const kinds = [...new Set(data.nodes.map(n => n.kind))].sort();
const legend = document.getElementById('legend');
const nodeLabel = document.createElement('div');
nodeLabel.style.cssText = 'color:#888; font-size:11px; margin-bottom:4px; margin-top:2px;';
nodeLabel.textContent = 'Nodes';
legend.appendChild(nodeLabel);
kinds.forEach(k => {
  const color = data.nodes.find(n => n.kind === k)?.color || '#999';
  const item = document.createElement('span');
  item.className = 'item';
  item.innerHTML = `<span class="dot" style="background:${color}"></span>${k}`;
  legend.appendChild(item);
});

// Build edge kind legend
const edgeKinds = [...new Set(data.links.map(l => l.kind))].sort();
if (edgeKinds.length > 0) {
  const edgeLabel = document.createElement('div');
  edgeLabel.style.cssText = 'color:#888; font-size:11px; margin-bottom:4px; margin-top:8px;';
  edgeLabel.textContent = 'Edges';
  legend.appendChild(edgeLabel);
  edgeKinds.forEach(k => {
    const color = data.links.find(l => l.kind === k)?.color || '#ccc';
    const item = document.createElement('span');
    item.className = 'item';
    item.innerHTML = `<span class="dot" style="background:${color}; border-radius:2px; width:16px; height:3px;"></span>${k}`;
    legend.appendChild(item);
  });
}

const svg = d3.select('svg');
const width = window.innerWidth;
const height = window.innerHeight;

// Zoom
const g = svg.append('g');
svg.call(d3.zoom()
  .scaleExtent([0.1, 8])
  .on('zoom', (event) => g.attr('transform', event.transform)));

// Node degree for sizing
const degree = {};
data.links.forEach(l => {
  degree[l.source] = (degree[l.source] || 0) + 1;
  degree[l.target] = (degree[l.target] || 0) + 1;
});

const simulation = d3.forceSimulation(data.nodes)
  .force('link', d3.forceLink(data.links).id(d => d.id).distance(80))
  .force('charge', d3.forceManyBody().strength(-150))
  .force('center', d3.forceCenter(width / 2, height / 2))
  .force('collision', d3.forceCollide().radius(d => nodeRadius(d) + 4));

function nodeRadius(d) {
  const deg = degree[d.id] || 0;
  return Math.max(4, Math.min(20, 4 + Math.sqrt(deg) * 2));
}

const link = g.append('g')
  .selectAll('line')
  .data(data.links)
  .join('line')
  .attr('class', 'link')
  .attr('stroke', d => d.color)
  .attr('stroke-width', 1);

const node = g.append('g')
  .selectAll('g')
  .data(data.nodes)
  .join('g')
  .attr('class', 'node')
  .call(d3.drag()
    .on('start', dragStarted)
    .on('drag', dragged)
    .on('end', dragEnded));

node.append('circle')
  .attr('r', d => nodeRadius(d))
  .attr('fill', d => d.color);

node.append('text')
  .attr('dx', d => nodeRadius(d) + 4)
  .attr('dy', 4)
  .text(d => d.name);

// Tooltip
const tooltip = document.getElementById('tooltip');

node.on('mouseover', (event, d) => {
  tooltip.style.display = 'block';
  tooltip.querySelector('.tt-name').textContent = d.name;
  tooltip.querySelector('.tt-kind').textContent = d.kind;
  const shortFile = d.file.replace(/.*[/]quantlane[/]/, '');
  tooltip.querySelector('.tt-file').textContent = `${shortFile}:${d.line}`;
})
.on('mousemove', (event) => {
  tooltip.style.left = (event.clientX + 14) + 'px';
  tooltip.style.top = (event.clientY - 10) + 'px';
})
.on('mouseout', () => {
  tooltip.style.display = 'none';
});

// Click to highlight neighborhood
node.on('click', (event, d) => {
  const neighbors = new Set([d.id]);
  data.links.forEach(l => {
    const src = typeof l.source === 'object' ? l.source.id : l.source;
    const tgt = typeof l.target === 'object' ? l.target.id : l.target;
    if (src === d.id) neighbors.add(tgt);
    if (tgt === d.id) neighbors.add(src);
  });

  node.classed('highlighted', n => n.id === d.id);
  node.classed('dimmed', n => !neighbors.has(n.id));
  link.classed('dimmed', l => {
    const src = typeof l.source === 'object' ? l.source.id : l.source;
    const tgt = typeof l.target === 'object' ? l.target.id : l.target;
    return !neighbors.has(src) || !neighbors.has(tgt);
  });

  event.stopPropagation();
});

// Click background to reset
svg.on('click', () => {
  node.classed('highlighted', false).classed('dimmed', false);
  link.classed('dimmed', false);
});

// Search
document.getElementById('search-box').addEventListener('input', (e) => {
  const q = e.target.value.toLowerCase();
  if (!q) {
    node.classed('dimmed', false);
    link.classed('dimmed', false);
    return;
  }
  const matching = new Set();
  data.nodes.forEach(n => {
    if (n.name.toLowerCase().includes(q) || n.kind.toLowerCase().includes(q)) {
      matching.add(n.id);
    }
  });
  node.classed('dimmed', n => !matching.has(n.id));
  link.classed('dimmed', l => {
    const src = typeof l.source === 'object' ? l.source.id : l.source;
    const tgt = typeof l.target === 'object' ? l.target.id : l.target;
    return !matching.has(src) && !matching.has(tgt);
  });
});

simulation.on('tick', () => {
  link
    .attr('x1', d => d.source.x).attr('y1', d => d.source.y)
    .attr('x2', d => d.target.x).attr('y2', d => d.target.y);
  node.attr('transform', d => `translate(${d.x},${d.y})`);
});

function dragStarted(event, d) {
  if (!event.active) simulation.alphaTarget(0.3).restart();
  d.fx = d.x; d.fy = d.y;
}
function dragged(event, d) { d.fx = event.x; d.fy = event.y; }
function dragEnded(event, d) {
  if (!event.active) simulation.alphaTarget(0);
  d.fx = null; d.fy = null;
}
</script>
</body>
</html>
"""
