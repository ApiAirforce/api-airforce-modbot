"use strict";

const app = document.getElementById("app");
const userbox = document.getElementById("userbox");

const SECTIONS = [
  ["link", "Link filter", "Delete non-whitelisted links, strike repeat offenders."],
  ["flood", "Flood / raid filter", "Catch cross-channel / burst / identical-content spam."],
  ["automod", "Content automod", "Blocklist, caps, mentions, emoji, zalgo, duplicates."],
  ["jail", "Jail", "Escape-proof role-snapshot quarantine."],
  ["raid", "Raid protection", "Join gate + join-velocity lockdown."],
  ["antinuke", "Anti-nuke", "Strip a rogue admin mass-deleting / mass-banning."],
  ["ai", "AI moderation", "Context-aware LLM moderation (model + policy per server)."],
  ["mod", "Mod-log & escalation", "Mod-log channel + warn auto-escalation."],
];

let state = { me: null, guildId: null, tab: "settings" };

init();

async function init() {
  const res = await fetch("/api/me");
  if (res.status === 200) {
    state.me = await res.json();
    renderUserbox();
    if (state.me.guilds.length) state.guildId = state.me.guilds[0].id;
    renderApp();
  } else {
    renderLogin();
  }
}

function renderLogin() {
  userbox.innerHTML = "";
  app.innerHTML = `
    <div class="login">
      <h1>🛡️ Modbot Dashboard</h1>
      <p>Log in with Discord to configure the servers you manage.</p>
      <p><a href="/api/login"><button>Log in with Discord</button></a></p>
    </div>`;
}

function renderUserbox() {
  userbox.innerHTML = `<span style="color:var(--muted);margin-right:12px">${esc(state.me.user.username)}</span>
    <button class="ghost small" id="logout">Log out</button>`;
  document.getElementById("logout").onclick = async () => {
    await fetch("/api/logout", { method: "POST" });
    location.reload();
  };
}

function renderApp() {
  if (!state.me.guilds.length) {
    app.innerHTML = `<div class="card"><h2>No manageable servers</h2>
      <p class="desc">You need <b>Manage Server</b> on a server the bot is in. Invite the bot, then reload.</p></div>`;
    return;
  }
  app.innerHTML = `
    <div class="cols">
      <nav class="guildlist">
        <h3>Your servers</h3>
        ${state.me.guilds.map(g => `
          <button class="guild ${g.id === state.guildId ? "active" : ""}" data-id="${esc(g.id)}">
            <span class="ico">${guildIcon(g)}</span><span>${esc(g.name)}</span>
          </button>`).join("")}
      </nav>
      <section>
        <div class="tabs">
          ${["settings", "cases", "strikes", "jails"].map(t =>
            `<button class="tab ${state.tab === t ? "active" : ""}" data-tab="${t}">${cap(t)}</button>`).join("")}
        </div>
        <div id="content"><p class="loading">Loading…</p></div>
      </section>
    </div>`;
  app.querySelectorAll(".guild").forEach(b => b.onclick = () => { state.guildId = b.dataset.id; renderApp(); });
  app.querySelectorAll(".tab").forEach(b => b.onclick = () => { state.tab = b.dataset.tab; renderTab(); });
  renderTab();
}

async function renderTab() {
  const c = document.getElementById("content");
  c.innerHTML = `<p class="loading">Loading…</p>`;
  if (state.tab === "settings") return renderSettings(c);
  return renderList(c, state.tab);
}

async function renderSettings(c) {
  const res = await fetch(`/api/guilds/${state.guildId}/config`);
  if (!res.ok) return (c.innerHTML = errBox(await res.json()));
  const cfg = await res.json();
  c.innerHTML = SECTIONS.map(([key, label, desc]) => `
    <div class="card" data-section="${key}">
      <h2>${label}</h2><p class="desc">${desc}</p>
      <div class="fields">${Object.entries(cfg[key]).map(([k, v]) => fieldRow(key, k, v)).join("")}</div>
      <div class="cardfoot"><button class="small" data-save="${key}">Save ${label}</button><span class="msg" id="msg-${key}"></span></div>
    </div>`).join("");
  c.querySelectorAll("[data-save]").forEach(b => b.onclick = () => saveSection(b.dataset.save));
}

function fieldRow(section, key, value) {
  const id = `f-${section}-${key}`;
  const label = `<label for="${id}">${prettify(key)}</label>`;
  let input;
  if (typeof value === "boolean") {
    input = `<input type="checkbox" id="${id}" data-type="bool" ${value ? "checked" : ""}/>`;
  } else if (typeof value === "number") {
    input = `<input type="number" id="${id}" data-type="number" value="${value}"/>`;
  } else if (typeof value === "string") {
    input = `<input type="text" id="${id}" data-type="string" value="${esc(value)}"/>`;
  } else if (Array.isArray(value) && value.every(x => typeof x === "string")) {
    input = `<textarea id="${id}" data-type="lines" placeholder="one per line">${esc(value.join("\n"))}</textarea>`;
  } else {
    input = `<textarea id="${id}" data-type="json">${esc(JSON.stringify(value, null, 2))}</textarea>`;
  }
  return `<div class="field">${label}${input}</div>`;
}

async function saveSection(section) {
  const msg = document.getElementById(`msg-${section}`);
  msg.className = "msg"; msg.textContent = "Saving…";
  const obj = {};
  let bad = null;
  document.querySelectorAll(`[data-section="${section}"] .field input, [data-section="${section}"] .field textarea`).forEach(el => {
    const key = el.id.replace(`f-${section}-`, "");
    try { obj[key] = readField(el); } catch (e) { bad = `${prettify(key)}: ${e.message}`; }
  });
  if (bad) { msg.className = "msg err"; msg.textContent = "Invalid: " + bad; return; }
  const res = await fetch(`/api/guilds/${state.guildId}/config/${section}`, {
    method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(obj),
  });
  if (res.ok) { msg.className = "msg ok"; msg.textContent = "✓ Saved"; }
  else { const e = await res.json(); msg.className = "msg err"; msg.textContent = "✗ " + (e.error || "failed"); }
}

function readField(el) {
  const t = el.dataset.type;
  if (t === "bool") return el.checked;
  if (t === "number") return Number(el.value);
  if (t === "string") return el.value;
  if (t === "lines") return el.value.split("\n").map(s => s.trim()).filter(Boolean);
  if (t === "json") return JSON.parse(el.value || "null");
  return el.value;
}

async function renderList(c, kind) {
  const res = await fetch(`/api/guilds/${state.guildId}/${kind}?limit=200`);
  if (!res.ok) return (c.innerHTML = errBox(await res.json()));
  const rows = await res.json();
  if (!rows.length) return (c.innerHTML = `<div class="card"><p class="empty">No ${kind} recorded.</p></div>`);
  const cols = {
    cases: ["id", "user_id", "mod_id", "action", "reason", "created_unix"],
    strikes: ["discord_user_id", "count", "last_reason", "last_strike_unix"],
    jails: ["discord_user_id", "reason", "jailed_by", "jailed_at_unix", "expires_at_unix"],
  }[kind];
  c.innerHTML = `<div class="card"><table>
    <thead><tr>${cols.map(h => `<th>${prettify(h)}</th>`).join("")}</tr></thead>
    <tbody>${rows.map(r => `<tr>${cols.map(h => `<td>${fmtCell(h, r[h])}</td>`).join("")}</tr>`).join("")}</tbody>
  </table></div>`;
}

// ── helpers ──────────────────────────────────────────────────────────────────
function fmtCell(h, v) {
  if (v === null || v === undefined) return "—";
  if (h.endsWith("_unix")) return esc(new Date(v * 1000).toLocaleString());
  if (h.endsWith("_id") || h === "id") return `<code>${esc(String(v))}</code>`;
  return esc(String(v));
}
function guildIcon(g) {
  if (g.icon) return `<img src="https://cdn.discordapp.com/icons/${esc(g.id)}/${esc(g.icon)}.png?size=32" alt=""/>`;
  return esc((g.name[0] || "?").toUpperCase());
}
function errBox(e) { return `<div class="card"><p class="msg err">${esc((e && e.error) || "Error")}</p></div>`; }
function prettify(k) { return k.replace(/_/g, " ").replace(/\b\w/g, c => c.toUpperCase()); }
function cap(s) { return s[0].toUpperCase() + s.slice(1); }
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])); }
