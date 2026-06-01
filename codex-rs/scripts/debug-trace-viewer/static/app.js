"use strict";

const state = {
  conversations: [],
  selectedConversationId: null,
  snapshot: null,
  metadata: null,
  autoRefresh: true,
  refreshIntervalMs: 2000,
  timerId: null,
  traceRoot: null,
};

const els = {
  conversationList: document.getElementById("conversationList"),
  conversationCount: document.getElementById("conversationCount"),
  traceContainer: document.getElementById("traceContainer"),
  statusBar: document.getElementById("statusBar"),
  activeTitle: document.getElementById("activeConversationTitle"),
  activeMeta: document.getElementById("activeConversationMeta"),
  statEntries: document.getElementById("statEntries"),
  statLanes: document.getElementById("statLanes"),
  statSchema: document.getElementById("statSchema"),
  autoRefresh: document.getElementById("autoRefresh"),
  refreshInterval: document.getElementById("refreshInterval"),
  refreshNow: document.getElementById("refreshNow"),
};

function escapeHtml(input) {
  return String(input ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function roleClass(role) {
  switch (String(role || "").toLowerCase()) {
    case "system":
      return "role-system";
    case "developer":
      return "role-developer";
    case "user":
      return "role-user";
    case "assistant":
      return "role-assistant";
    case "tool":
      return "role-tool";
    default:
      return "";
  }
}

function formatTimestamp(entry) {
  const raw = entry.timestamp_rfc3339_sec;
  if (typeof raw === "string" && raw.length > 0) {
    const d = new Date(raw);
    if (!Number.isNaN(d.getTime())) {
      const hh = String(d.getHours()).padStart(2, "0");
      const mm = String(d.getMinutes()).padStart(2, "0");
      const ss = String(d.getSeconds()).padStart(2, "0");
      return `${hh}:${mm}:${ss}`;
    }
  }
  if (typeof entry.timestamp_unix_sec === "number") {
    const d = new Date(entry.timestamp_unix_sec * 1000);
    const hh = String(d.getHours()).padStart(2, "0");
    const mm = String(d.getMinutes()).padStart(2, "0");
    const ss = String(d.getSeconds()).padStart(2, "0");
    return `${hh}:${mm}:${ss}`;
  }
  return "-";
}

function formatConversationTime(unixSec, rfc3339) {
  if (typeof rfc3339 === "string" && rfc3339) {
    const d = new Date(rfc3339);
    if (!Number.isNaN(d.getTime())) {
      return d.toLocaleString();
    }
  }
  if (typeof unixSec === "number") {
    return new Date(unixSec * 1000).toLocaleString();
  }
  return "-";
}

async function fetchJson(url) {
  const response = await fetch(url, { cache: "no-store" });
  if (!response.ok) {
    const body = await response.text();
    throw new Error(`HTTP ${response.status}: ${body}`);
  }
  return response.json();
}

function setStatus(message, isError = false) {
  if (!message) {
    els.statusBar.textContent = "";
    els.statusBar.className = "status-bar";
    return;
  }
  els.statusBar.textContent = message;
  els.statusBar.className = isError ? "status-bar status-error" : "status-bar";
}

function updateStats(snapshot) {
  if (!snapshot) {
    els.statEntries.textContent = "-";
    els.statLanes.textContent = "-";
    els.statSchema.textContent = "-";
    return;
  }
  const entries = Array.isArray(snapshot.entries) ? snapshot.entries.length : 0;
  const lanes = Array.isArray(snapshot.lanes) ? snapshot.lanes.length : 0;
  els.statEntries.textContent = String(entries);
  els.statLanes.textContent = String(lanes);
  els.statSchema.textContent = snapshot.schema_version || "-";
}

function renderConversationList() {
  const list = state.conversations;
  els.conversationCount.textContent = String(list.length);
  if (list.length === 0) {
    els.conversationList.innerHTML =
      '<div class="trace-empty">No conversation traces found.</div>';
    return;
  }

  const html = list
    .map((item) => {
      const active = item.conversationId === state.selectedConversationId ? "active" : "";
      const updated = formatConversationTime(item.updatedAtUnixSec, item.updatedAtRfc3339Sec);
      const entryInfo =
        typeof item.entryCount === "number" ? `${item.entryCount} entries` : "entries: -";
      const laneInfo =
        typeof item.laneCount === "number" ? `${item.laneCount} agents` : "agents: -";
      return `
        <button class="conversation-item ${active}" data-conversation-id="${escapeHtml(item.conversationId)}">
          <div class="conversation-id">${escapeHtml(item.conversationId)}</div>
          <div class="conversation-meta">${escapeHtml(updated)}</div>
          <div class="conversation-meta">${escapeHtml(entryInfo)} · ${escapeHtml(laneInfo)}</div>
        </button>
      `;
    })
    .join("");

  els.conversationList.innerHTML = html;

  for (const button of els.conversationList.querySelectorAll(".conversation-item")) {
    button.addEventListener("click", async () => {
      const id = button.getAttribute("data-conversation-id");
      if (!id || id === state.selectedConversationId) {
        return;
      }
      state.selectedConversationId = id;
      renderConversationList();
      await loadSnapshot(id);
    });
  }
}

function normalizeLanes(snapshot) {
  const lanes = Array.isArray(snapshot.lanes) ? [...snapshot.lanes] : [];
  const entries = Array.isArray(snapshot.entries) ? snapshot.entries : [];
  lanes.sort((a, b) => (a.lane_index ?? 0) - (b.lane_index ?? 0));

  if (lanes.length > 0) {
    return lanes.map((lane, idx) => ({
      agent_thread_id: lane.agent_thread_id,
      agent_name: lane.agent_name || lane.agent_thread_id || `agent-${idx + 1}`,
      lane_index: typeof lane.lane_index === "number" ? lane.lane_index : idx,
      lane_position: lane.lane_position || "scroll",
      agent_kind: lane.agent_kind || null,
    }));
  }

  const seen = new Map();
  for (const entry of entries) {
    const key = entry.agent_thread_id || `unknown-${seen.size + 1}`;
    if (seen.has(key)) {
      continue;
    }
    seen.set(key, {
      agent_thread_id: key,
      agent_name: entry.agent_name || key,
      lane_index: seen.size,
      lane_position: "scroll",
      agent_kind: entry.agent_kind || null,
    });
  }
  return Array.from(seen.values());
}

function lanePositionLabel(position) {
  const p = String(position || "").toLowerCase();
  if (!p) return "scroll";
  return p;
}

function buildParallelRows(snapshot, lanes) {
  const entries = Array.isArray(snapshot.entries) ? [...snapshot.entries] : [];
  entries.sort((a, b) => {
    const aSec = Number.isFinite(a.timestamp_unix_sec) ? a.timestamp_unix_sec : 0;
    const bSec = Number.isFinite(b.timestamp_unix_sec) ? b.timestamp_unix_sec : 0;
    if (aSec !== bSec) return aSec - bSec;
    const aSeq = Number.isFinite(a.entry_seq) ? a.entry_seq : 0;
    const bSeq = Number.isFinite(b.entry_seq) ? b.entry_seq : 0;
    return aSeq - bSeq;
  });

  const laneByIndex = new Map(lanes.map((l) => [l.lane_index, l]));
  const laneByThread = new Map(lanes.map((l) => [l.agent_thread_id, l]));
  const rowsBySecond = new Map();

  for (const entry of entries) {
    const sec = Number.isFinite(entry.timestamp_unix_sec) ? entry.timestamp_unix_sec : -1;
    if (!rowsBySecond.has(sec)) {
      rowsBySecond.set(sec, { sec, entriesByLane: new Map(), firstEntry: entry });
    }
    const row = rowsBySecond.get(sec);

    let lane = null;
    if (Number.isFinite(entry.lane_index)) {
      lane = laneByIndex.get(entry.lane_index) || null;
    }
    if (!lane && entry.agent_thread_id) {
      lane = laneByThread.get(entry.agent_thread_id) || null;
    }
    if (!lane) {
      lane = lanes[0] || null;
    }
    if (!lane) {
      continue;
    }

    const laneKey = lane.agent_thread_id;
    if (!row.entriesByLane.has(laneKey)) {
      row.entriesByLane.set(laneKey, []);
    }
    row.entriesByLane.get(laneKey).push(entry);
  }

  const rows = Array.from(rowsBySecond.values());
  rows.sort((a, b) => a.sec - b.sec);
  return rows;
}

function renderParallelTable(snapshot, lanes) {
  if (lanes.length === 0) {
    return '<div class="trace-empty">No lane data available.</div>';
  }

  const rows = buildParallelRows(snapshot, lanes);
  const timeColWidth = 110;
  const laneColWidth = lanes.length <= 3 ? 320 : 300;
  const totalMinWidth = timeColWidth + lanes.length * laneColWidth;
  const gridColumns = `${timeColWidth}px repeat(${lanes.length}, minmax(${laneColWidth}px, 1fr))`;

  const headerCells = lanes
    .map((lane) => {
      const position = lanePositionLabel(lane.lane_position);
      return `
        <div class="trace-head-cell">
          <div class="lane-head-title">${escapeHtml(lane.agent_name)}</div>
          <div class="lane-head-sub">lane ${lane.lane_index} · ${escapeHtml(position)}</div>
        </div>
      `;
    })
    .join("");

  const bodyRows = rows
    .map((row) => {
      const ts = formatTimestamp(row.firstEntry);
      const laneCells = lanes
        .map((lane) => {
          const laneEntries = row.entriesByLane.get(lane.agent_thread_id) || [];
          if (laneEntries.length === 0) {
            return '<div class="trace-cell lane-cell"></div>';
          }
          const cards = laneEntries
            .map((entry) => {
              const role = String(entry.role || "").toLowerCase();
              const roleCls = roleClass(role);
              return `
                <article class="entry-card">
                  <div class="entry-meta">
                    <span class="role-badge ${roleCls}">${escapeHtml(role || "unknown")}</span>
                    <span class="entry-agent">${escapeHtml(entry.agent_name || entry.agent_thread_id || "-")}</span>
                    <span class="entry-seq">#${escapeHtml(entry.entry_seq ?? "-")}</span>
                  </div>
                  <div class="entry-source">${escapeHtml(entry.source_event || "-")}</div>
                  <div class="entry-content">${escapeHtml(entry.content || "")}</div>
                </article>
              `;
            })
            .join("");
          return `<div class="trace-cell lane-cell">${cards}</div>`;
        })
        .join("");

      return `
        <div class="trace-row" style="grid-template-columns:${gridColumns}">
          <div class="trace-cell time-cell">${escapeHtml(ts)}</div>
          ${laneCells}
        </div>
      `;
    })
    .join("");

  return `
    <div class="trace-scroll">
      <div class="trace-table" style="min-width:${totalMinWidth}px">
        <div class="trace-head-row" style="grid-template-columns:${gridColumns}">
          <div class="trace-head-cell time-cell">Time</div>
          ${headerCells}
        </div>
        ${bodyRows}
      </div>
    </div>
  `;
}

function renderSnapshot() {
  const snapshot = state.snapshot;
  const metadata = state.metadata;
  if (!snapshot) {
    els.activeTitle.textContent = "No conversation selected";
    els.activeMeta.textContent = "";
    els.traceContainer.innerHTML =
      '<div class="trace-empty">Select a conversation from the left panel.</div>';
    updateStats(null);
    return;
  }

  const lanes = normalizeLanes(snapshot);
  const generated = formatConversationTime(
    snapshot.generated_at_unix_sec,
    snapshot.generated_at_rfc3339_sec
  );
  const updated = metadata
    ? formatConversationTime(metadata.updated_at_unix_sec, metadata.updated_at_rfc3339_sec)
    : generated;

  els.activeTitle.textContent = state.selectedConversationId;
  els.activeMeta.textContent = `Generated: ${generated} · Updated: ${updated} · Root: ${
    state.traceRoot || "-"
  }`;
  updateStats(snapshot);
  els.traceContainer.innerHTML = renderParallelTable(snapshot, lanes);
}

async function loadConversations() {
  const data = await fetchJson("/api/conversations");
  const conversations = Array.isArray(data.conversations) ? data.conversations : [];
  state.conversations = conversations;
  state.traceRoot = data.traceRoot || state.traceRoot;

  if (!state.selectedConversationId && conversations.length > 0) {
    state.selectedConversationId = conversations[0].conversationId;
  }

  const stillExists = conversations.some(
    (item) => item.conversationId === state.selectedConversationId
  );
  if (!stillExists) {
    state.selectedConversationId = conversations.length > 0 ? conversations[0].conversationId : null;
  }
}

async function loadSnapshot(conversationId) {
  if (!conversationId) {
    state.snapshot = null;
    state.metadata = null;
    renderSnapshot();
    return;
  }

  const data = await fetchJson(
    `/api/conversations/${encodeURIComponent(conversationId)}/snapshot`
  );
  state.snapshot = data.snapshot || null;
  state.metadata = data.metadata || null;
  renderSnapshot();
}

async function refreshAll() {
  try {
    await loadConversations();
    renderConversationList();
    if (state.selectedConversationId) {
      await loadSnapshot(state.selectedConversationId);
    } else {
      renderSnapshot();
    }
    setStatus(`Last refresh: ${new Date().toLocaleTimeString()}`);
  } catch (err) {
    setStatus(String(err), true);
  }
}

function restartTimer() {
  if (state.timerId !== null) {
    window.clearInterval(state.timerId);
    state.timerId = null;
  }
  if (!state.autoRefresh) {
    return;
  }
  state.timerId = window.setInterval(() => {
    refreshAll();
  }, state.refreshIntervalMs);
}

function bindUi() {
  els.autoRefresh.addEventListener("change", () => {
    state.autoRefresh = els.autoRefresh.checked;
    restartTimer();
  });

  els.refreshInterval.addEventListener("change", () => {
    state.refreshIntervalMs = Number(els.refreshInterval.value) || 2000;
    restartTimer();
  });

  els.refreshNow.addEventListener("click", () => {
    refreshAll();
  });
}

async function bootstrap() {
  bindUi();
  await refreshAll();
  restartTimer();
}

bootstrap();

