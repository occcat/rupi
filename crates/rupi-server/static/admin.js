const TOKEN_KEY = "rupi_admin_token";

const $ = (id) => document.getElementById(id);
const view = () => $("view");

function token() {
  return sessionStorage.getItem(TOKEN_KEY) || "";
}

function toast(msg, bad) {
  const el = $("toast");
  el.hidden = false;
  el.classList.toggle("bad", !!bad);
  el.textContent = msg;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => { el.hidden = true; }, 4200);
}

async function api(path, opts = {}) {
  const headers = Object.assign({ Accept: "application/json" }, opts.headers || {});
  const t = token();
  if (t) headers.Authorization = "Bearer " + t;
  let body = opts.body;
  if (body && typeof body !== "string") {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(body);
  }
  const res = await fetch(path, { method: opts.method || "GET", headers, body });
  if (res.status === 401) {
    sessionStorage.removeItem(TOKEN_KEY);
    showLogin("凭证无效");
    throw new Error("unauthorized");
  }
  if (res.status === 204) return null;
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = { raw: text }; }
  if (!res.ok) throw new Error((data && data.error) || (res.status + " " + res.statusText));
  return data;
}

function showLogin(err) {
  $("login").hidden = false;
  $("shell").hidden = true;
  const box = $("login-err");
  box.hidden = !err;
  box.textContent = err || "";
}

function showShell() {
  $("login").hidden = true;
  $("shell").hidden = false;
}

function closeModal() {
  $("modal").hidden = true;
  $("modal-body").innerHTML = "";
}

function openModal(title, html) {
  $("modal-title").textContent = title;
  $("modal-body").innerHTML = html;
  $("modal").hidden = false;
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"
  }[c]));
}

function when(v) {
  if (!v) return "—";
  const d = new Date(v);
  return Number.isNaN(d.getTime()) ? String(v) : d.toLocaleString();
}

function badge(status) {
  return `<span class="badge ${esc(status)}">${esc(status)}</span>`;
}

function parseHash() {
  const h = (location.hash || "#/").replace(/^#/, "") || "/";
  const parts = h.split("/").filter(Boolean);
  if (!parts.length) return { name: "overview" };
  if (parts[0] === "tenants" && parts[1]) return { name: "tenant", id: parts[1] };
  if (parts[0] === "tenants") return { name: "tenants" };
  if (parts[0] === "sessions") return { name: "sessions" };
  if (parts[0] === "executors") return { name: "executors" };
  if (parts[0] === "keys") return { name: "keys" };
  return { name: "overview" };
}

function setNav(name) {
  document.querySelectorAll("[data-nav]").forEach((a) => {
    a.classList.toggle("active", a.dataset.nav === name || (name === "tenant" && a.dataset.nav === "tenants"));
  });
}

async function refreshPills() {
  try {
    const o = await api("/admin/api/overview");
    $("pills").innerHTML = [
      pill("实例", o.instanceId),
      pill("区域", o.region),
      pill("Postgres", o.postgres ? "ok" : "down", o.postgres),
      pill("Redis", o.redis ? "ok" : "降级", o.redis),
    ].join("");
  } catch (_) { /* 登录页会处理 */ }
}

function pill(k, v, ok) {
  const cls = ok === undefined ? "" : ok ? "ok" : "bad";
  return `<span class="pill ${cls}">${esc(k)} ${esc(v)}</span>`;
}

async function render() {
  closeModal();
  if (!token()) {
    showLogin();
    return;
  }
  showShell();
  const r = parseHash();
  setNav(r.name);
  await refreshPills();
  const titles = {
    overview: ["概览", "控制面健康、配额账本与执行池容量。"],
    tenants: ["租户 / API Key", "创建、列表、吊销。明文 Key 只出现一次。"],
    tenant: ["租户详情", "配额、settings、会话与 Key。"],
    sessions: ["会话目录", "按租户 / 区域 / 状态查看，可删除。"],
    executors: ["区域 / 执行池", "execd / sandboxd 容量与句柄。"],
    keys: ["管理凭证", "独立 admin 口令或高权限 Key，无 OAuth。"],
  };
  const t = titles[r.name] || titles.overview;
  $("page-title").textContent = t[0];
  $("page-sub").textContent = t[1];
  try {
    if (r.name === "tenants") await pageTenants();
    else if (r.name === "tenant") await pageTenant(r.id);
    else if (r.name === "sessions") await pageSessions();
    else if (r.name === "executors") await pageExecutors();
    else if (r.name === "keys") await pageKeys();
    else await pageOverview();
  } catch (e) {
    if (e.message !== "unauthorized") {
      view().innerHTML = `<p class="err">${esc(e.message)}</p>`;
    }
  }
}

async function pageOverview() {
  const o = await api("/admin/api/overview");
  const c = o.counts || {};
  const pool = o.pool || {};
  const used = pool.used || 0;
  const cap = pool.capacity || 0;
  const pct = cap ? Math.min(100, Math.round((used / cap) * 100)) : 0;
  view().innerHTML = `
    <section class="cards">
      ${card("租户", c.tenants)}
      ${card("会话", c.sessions)}
      ${card("运行中", c.running)}
      ${card("已分配句柄", c.allocated)}
      ${card("快照", c.snapshotted)}
      ${card("有效 Key", c.keysActive)}
    </section>
    <section class="card">
      <div class="k">执行池</div>
      <div class="v">${used} / ${cap || "—"}</div>
      <div class="bar" style="margin-top:.6rem"><i style="width:${pct}%"></i></div>
      <p class="muted">节点 ${esc((o.topology || []).length)} · warm ${esc(pool.warm ?? "—")}</p>
    </section>`;
}

function card(k, v) {
  return `<article class="card"><div class="k">${esc(k)}</div><div class="v">${esc(v ?? 0)}</div></article>`;
}

async function pageTenants() {
  const data = await api("/admin/api/tenants?limit=100");
  const rows = (data.tenants || []).map((t) => `
    <tr>
      <td><a href="#/tenants/${esc(t.id)}">${esc(t.name)}</a></td>
      <td class="mono">${esc(t.id)}</td>
      <td>${esc(t.defaultRegion || "—")}</td>
      <td>${esc(t.sessionCount)}</td>
      <td>${esc(t.keyCount)}</td>
      <td>${esc(t.quota?.maxConcurrentRuns)} / ${esc(t.quota?.maxHandles)} / qps ${esc(t.quota?.maxQps)}</td>
    </tr>`).join("");
  view().innerHTML = `
    <div class="toolbar">
      <button type="button" id="btn-new-tenant">新建租户</button>
    </div>
    <div class="table-wrap">
      <table>
        <thead><tr><th>名称</th><th>ID</th><th>默认区域</th><th>会话</th><th>Key</th><th>配额</th></tr></thead>
        <tbody>${rows || `<tr><td colspan="6" class="empty">还没有租户</td></tr>`}</tbody>
      </table>
    </div>`;
  $("btn-new-tenant").onclick = () => {
    openModal("新建租户", `
      <form id="f-tenant" class="toolbar" style="display:grid;gap:.7rem">
        <label>名称 <input name="name" required></label>
        <label>默认模型 <input name="defaultModel" placeholder="gpt-4o-mini"></label>
        <label>默认区域 <input name="defaultRegion" value="local" placeholder="local"></label>
        <button type="submit">创建并颁发 Key</button>
      </form>`);
    $("f-tenant").onsubmit = async (ev) => {
      ev.preventDefault();
      const fd = new FormData(ev.target);
      try {
        const created = await api("/admin/api/tenants", {
          method: "POST",
          body: {
            name: fd.get("name"),
            defaultModel: fd.get("defaultModel") || null,
            defaultRegion: fd.get("defaultRegion") || null,
          },
        });
        closeModal();
        showOnce("租户已创建，请立刻复制 Key", created.key);
        location.hash = "#/tenants/" + created.tenant.id;
      } catch (e) { toast(e.message, true); }
    };
  };
}

function showOnce(title, key) {
  openModal(title, `
    <p class="warn">明文只显示这一次，关闭后无法再看。</p>
    <p class="secret" id="once-key">${esc(key)}</p>
    <p><button type="button" id="copy-key">复制</button></p>`);
  $("copy-key").onclick = async () => {
    try { await navigator.clipboard.writeText(key); toast("已复制"); }
    catch { toast("复制失败，请手动选中", true); }
  };
}

async function pageTenant(id) {
  const t = await api("/admin/api/tenants/" + encodeURIComponent(id));
  const q = t.quota || {};
  const keys = (t.keys || []).map((k) => `
    <tr>
      <td class="mono">${esc(k.prefix)}…</td>
      <td>${when(k.createdAt)}</td>
      <td>${k.revoked ? badge("revoked") : badge("ready")}</td>
      <td>${k.revoked ? "" : `<button type="button" class="danger" data-revoke="${esc(k.id)}">吊销</button>`}</td>
    </tr>`).join("");
  const settings = t.settings || {};
  view().innerHTML = `
    <section class="cards">
      ${card("会话", t.sessionCount)}
      ${card("今日 runs", q.runsToday)}
      ${card("今日 tokens", q.tokensToday)}
      ${card("并发 / 句柄", (q.maxConcurrentRuns ?? "—") + " / " + (q.maxHandles ?? "—"))}
    </section>
    <section class="split">
      <form id="f-meta" class="card" style="display:grid;gap:.6rem">
        <strong>租户</strong>
        <label>名称 <input name="name" value="${esc(t.name)}"></label>
        <label>默认模型 <input name="defaultModel" value="${esc(t.defaultModel || "")}"></label>
        <label>默认区域 <input name="defaultRegion" value="${esc(t.defaultRegion || "")}"></label>
        <button type="submit">保存元数据</button>
      </form>
      <form id="f-quota" class="card" style="display:grid;gap:.6rem">
        <strong>配额 / 限流</strong>
        <label>并发 run <input name="maxConcurrentRuns" type="number" min="0" value="${esc(q.maxConcurrentRuns)}"></label>
        <label>句柄 <input name="maxHandles" type="number" min="0" value="${esc(q.maxHandles)}"></label>
        <label>日 runs <input name="maxRunsPerDay" type="number" min="0" value="${esc(q.maxRunsPerDay)}"></label>
        <label>日 tokens <input name="maxTokensPerDay" type="number" min="0" value="${esc(q.maxTokensPerDay)}"></label>
        <label>QPS <input name="maxQps" type="number" min="0" value="${esc(q.maxQps)}"></label>
        <button type="submit">调整配额</button>
      </form>
    </section>
    <section class="card">
      <div class="toolbar">
        <strong>API Key</strong>
        <button type="button" id="btn-new-key">颁发新 Key</button>
      </div>
      <div class="table-wrap">
        <table>
          <thead><tr><th>前缀</th><th>创建</th><th>状态</th><th></th></tr></thead>
          <tbody>${keys || `<tr><td colspan="4" class="empty">无 Key</td></tr>`}</tbody>
        </table>
      </div>
    </section>
    <form id="f-settings" class="card" style="display:grid;gap:.6rem">
      <strong>Settings</strong>
      <p class="muted">只允许现有 JSON 键。密钥显示为 ***，提交 *** 不会覆盖。</p>
      <label>JSON
        <textarea name="json">${esc(JSON.stringify(settings, null, 2))}</textarea>
      </label>
      <p class="muted">允许：${esc((t.allowedKeys || []).join(", "))}</p>
      <button type="submit">有限 PATCH</button>
    </form>
    <p><a href="#/sessions?tenantId=${encodeURIComponent(id)}">查看该租户会话 →</a></p>`;

  $("f-meta").onsubmit = async (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    try {
      await api("/admin/api/tenants/" + id, {
        method: "PATCH",
        body: {
          name: fd.get("name"),
          defaultModel: fd.get("defaultModel") || null,
          defaultRegion: fd.get("defaultRegion") || null,
        },
      });
      toast("租户已保存");
      render();
    } catch (e) { toast(e.message, true); }
  };
  $("f-quota").onsubmit = async (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    const num = (k) => {
      const v = fd.get(k);
      return v === "" || v == null ? null : Number(v);
    };
    try {
      await api("/admin/api/tenants/" + id + "/quota", {
        method: "PATCH",
        body: {
          maxConcurrentRuns: num("maxConcurrentRuns"),
          maxHandles: num("maxHandles"),
          maxRunsPerDay: num("maxRunsPerDay"),
          maxTokensPerDay: num("maxTokensPerDay"),
          maxQps: num("maxQps"),
        },
      });
      toast("配额已更新");
      render();
    } catch (e) { toast(e.message, true); }
  };
  $("btn-new-key").onclick = async () => {
    try {
      const created = await api("/admin/api/tenants/" + id + "/keys", { method: "POST" });
      showOnce("新 Key（只此一次）", created.key);
      $("modal-close").addEventListener("click", () => render(), { once: true });
    } catch (e) { toast(e.message, true); }
  };
  view().querySelectorAll("[data-revoke]").forEach((btn) => {
    btn.onclick = async () => {
      if (!confirm("吊销后该 Key 立刻不能访问 /v1。")) return;
      try {
        await api("/admin/api/keys/" + btn.dataset.revoke + "/revoke", { method: "POST" });
        toast("已吊销");
        render();
      } catch (e) { toast(e.message, true); }
    };
  });
  $("f-settings").onsubmit = async (ev) => {
    ev.preventDefault();
    try {
      const parsed = JSON.parse(ev.target.json.value);
      await api("/admin/api/tenants/" + id + "/settings", { method: "PATCH", body: parsed });
      toast("settings 已更新");
      render();
    } catch (e) { toast(e.message, true); }
  };
}

function hashQuery() {
  const raw = (location.hash.split("?")[1] || "");
  return new URLSearchParams(raw);
}

async function pageSessions() {
  const qs = hashQuery();
  const tenantId = qs.get("tenantId") || "";
  const region = qs.get("region") || "";
  const status = qs.get("status") || "";
  const params = new URLSearchParams();
  if (tenantId) params.set("tenantId", tenantId);
  if (region) params.set("region", region);
  if (status) params.set("status", status);
  params.set("limit", "100");
  const data = await api("/admin/api/sessions?" + params.toString());
  const regions = await api("/admin/api/regions");
  const regionOpts = ["", ...(regions.executorRegions || []), ...(regions.sessionRegions || [])]
    .filter((v, i, a) => a.indexOf(v) === i);
  const rows = (data.sessions || []).map((s) => `
    <tr>
      <td class="mono"><a href="#/sessions?open=${encodeURIComponent(s.id)}">${esc(s.id.slice(0, 8))}</a></td>
      <td class="mono">${esc((s.tenantId || "").slice(0, 8))}</td>
      <td>${esc(s.name || "—")}</td>
      <td>${esc(s.region || "—")}</td>
      <td>${badge(s.status)}</td>
      <td class="mono">${esc(s.runtime?.backend || "—")}<br>${esc(s.runtime?.handle || "")}</td>
      <td>${when(s.updatedAt)}</td>
      <td class="row-actions">
        <button type="button" data-open="${esc(s.id)}">详情</button>
        <button type="button" class="danger" data-del="${esc(s.id)}">删除</button>
      </td>
    </tr>`).join("");
  view().innerHTML = `
    <form id="f-sess" class="toolbar">
      <label>租户 ID <input name="tenantId" value="${esc(tenantId)}"></label>
      <label>区域
        <select name="region">
          ${regionOpts.map((r) => `<option value="${esc(r)}" ${r === region ? "selected" : ""}>${esc(r || "全部")}</option>`).join("")}
        </select>
      </label>
      <label>状态
        <select name="status">
          ${["", "running", "ready", "hot", "snapshotted", "none"].map((s) =>
            `<option value="${s}" ${s === status ? "selected" : ""}>${s || "全部"}</option>`).join("")}
        </select>
      </label>
      <button type="submit">筛选</button>
    </form>
    <p class="muted">共 ${esc(data.total)} 条</p>
    <div class="table-wrap">
      <table>
        <thead><tr><th>会话</th><th>租户</th><th>名称</th><th>区域</th><th>状态</th><th>句柄</th><th>更新</th><th></th></tr></thead>
        <tbody>${rows || `<tr><td colspan="8" class="empty">没有会话</td></tr>`}</tbody>
      </table>
    </div>
    <div id="sess-detail"></div>`;
  $("f-sess").onsubmit = (ev) => {
    ev.preventDefault();
    const fd = new FormData(ev.target);
    const p = new URLSearchParams();
    if (fd.get("tenantId")) p.set("tenantId", fd.get("tenantId"));
    if (fd.get("region")) p.set("region", fd.get("region"));
    if (fd.get("status")) p.set("status", fd.get("status"));
    location.hash = "#/sessions" + (p.toString() ? "?" + p.toString() : "");
  };
  view().querySelectorAll("[data-del]").forEach((btn) => {
    btn.onclick = async () => {
      if (!confirm("删除会话并释放 Executor 卷？")) return;
      try {
        await api("/admin/api/sessions/" + btn.dataset.del, { method: "DELETE" });
        toast("已删除");
        render();
      } catch (e) { toast(e.message, true); }
    };
  });
  view().querySelectorAll("[data-open]").forEach((btn) => {
    btn.onclick = () => openSession(btn.dataset.open);
  });
  const openId = qs.get("open");
  if (openId) openSession(openId);
}

async function openSession(id) {
  const box = $("sess-detail");
  if (!box) return;
  try {
    const s = await api("/admin/api/sessions/" + encodeURIComponent(id));
    const hint = s.openHint || {};
    box.innerHTML = `
      <section class="card">
        <strong>会话 ${esc(s.id)}</strong>
        <p class="muted">租户 ${esc(s.tenantId)} · ${badge(s.status)} · ${esc(s.runtime?.backend || "无后端")}</p>
        <div class="hint">
          <div>打开某会话：对话仍走 AG-UI，不是本控制台。</div>
          <pre>${esc(JSON.stringify(hint.bodyExample, null, 2))}</pre>
          <pre>${esc(hint.cli || "")}</pre>
        </div>
        <form id="f-debug" style="display:grid;gap:.55rem;margin-top:.8rem">
          <label>最小 prompt 调试（运营试跑，不是 IDE）
            <textarea name="prompt" placeholder="hello"></textarea>
          </label>
          <button type="submit">试跑一轮</button>
        </form>
        <pre id="debug-out" class="hint" hidden></pre>
      </section>`;
    $("f-debug").onsubmit = async (ev) => {
      ev.preventDefault();
      const prompt = new FormData(ev.target).get("prompt");
      const out = $("debug-out");
      out.hidden = false;
      out.textContent = "连接中…";
      try {
        const res = await fetch("/admin/api/sessions/" + encodeURIComponent(id) + "/debug-run", {
          method: "POST",
          headers: {
            Authorization: "Bearer " + token(),
            "Content-Type": "application/json",
            Accept: "text/event-stream",
          },
          body: JSON.stringify({ prompt }),
        });
        if (!res.ok) {
          out.textContent = await res.text();
          return;
        }
        const reader = res.body.getReader();
        const dec = new TextDecoder();
        let buf = "";
        let acc = "";
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          const parts = buf.split("\n\n");
          buf = parts.pop() || "";
          for (const part of parts) {
            const line = part.split("\n").find((l) => l.startsWith("data:"));
            if (line) acc += line.slice(5).trim() + "\n";
          }
          out.textContent = acc || "(无事件)";
        }
      } catch (e) { out.textContent = e.message; }
    };
  } catch (e) { toast(e.message, true); }
}

async function pageExecutors() {
  const ex = await api("/admin/api/executors");
  const nodes = (ex.nodes || []).map((n) => {
    const cap = n.capacity || 0;
    const used = n.used || 0;
    const pct = cap ? Math.min(100, Math.round((used / cap) * 100)) : 0;
    return `
      <article class="card">
        <div class="k">${esc(n.kind || n.backend)} · ${esc(n.region || "—")}</div>
        <div class="v">${esc(n.nodeId || n.node_id || n.backend)}</div>
        <p class="muted">used ${used} / cap ${cap} · warm ${esc(n.warm ?? 0)}</p>
        <div class="bar"><i style="width:${pct}%"></i></div>
      </article>`;
  }).join("");
  const handles = (ex.handles || []).map((h) => `
    <tr>
      <td>${esc(h.backend)}</td>
      <td>${esc(h.kind)}</td>
      <td>${esc(h.region)}</td>
      <td>${esc(h.allocated)}</td>
      <td>${esc(h.hot)}</td>
      <td>${esc(h.snapshotted)}</td>
    </tr>`).join("");
  view().innerHTML = `
    <p class="muted">控制面 ${esc(ex.controlPlane?.instanceId)} @ ${esc(ex.controlPlane?.region)}</p>
    <section class="cards">${nodes || `<article class="card"><div class="k">无执行节点</div></article>`}</section>
    <div class="table-wrap">
      <table>
        <thead><tr><th>backend</th><th>kind</th><th>区域</th><th>已分配</th><th>hot</th><th>快照</th></tr></thead>
        <tbody>${handles || `<tr><td colspan="6" class="empty">没有句柄</td></tr>`}</tbody>
      </table>
    </div>`;
}

async function pageKeys() {
  const data = await api("/admin/api/admin-keys");
  const rows = (data.keys || []).map((k) => `
    <tr>
      <td class="mono">${esc(k.prefix)}…</td>
      <td>${when(k.createdAt)}</td>
      <td>${k.revoked ? badge("revoked") : badge("ready")}</td>
      <td>${k.revoked ? "" : `<button type="button" class="danger" data-arevoke="${esc(k.id)}">吊销</button>`}</td>
    </tr>`).join("");
  view().innerHTML = `
    <p class="muted">环境口令已配置：${data.envTokenConfigured ? "是" : "否"}。也可用下方高权限 Key。</p>
    <p><button type="button" id="btn-adminkey">颁发管理 Key</button></p>
    <div class="table-wrap">
      <table>
        <thead><tr><th>前缀</th><th>创建</th><th>状态</th><th></th></tr></thead>
        <tbody>${rows || `<tr><td colspan="4" class="empty">还没有管理 Key</td></tr>`}</tbody>
      </table>
    </div>`;
  $("btn-adminkey").onclick = async () => {
    try {
      const created = await api("/admin/api/admin-keys", { method: "POST" });
      showOnce("管理 Key（只此一次）", created.key);
      $("modal-close").addEventListener("click", () => render(), { once: true });
    } catch (e) { toast(e.message, true); }
  };
  view().querySelectorAll("[data-arevoke]").forEach((btn) => {
    btn.onclick = async () => {
      if (!confirm("吊销这把管理 Key？")) return;
      try {
        await api("/admin/api/admin-keys/" + btn.dataset.arevoke + "/revoke", { method: "POST" });
        toast("已吊销");
        render();
      } catch (e) { toast(e.message, true); }
    };
  });
}

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const value = $("login-token").value.trim();
  sessionStorage.setItem(TOKEN_KEY, value);
  try {
    await api("/admin/api/me");
    render();
  } catch (e) {
    sessionStorage.removeItem(TOKEN_KEY);
    showLogin(e.message === "unauthorized" ? "凭证无效" : e.message);
  }
});

$("logout").addEventListener("click", () => {
  sessionStorage.removeItem(TOKEN_KEY);
  showLogin();
});
$("modal-close").addEventListener("click", closeModal);
$("modal").addEventListener("click", (ev) => {
  if (ev.target === $("modal")) closeModal();
});
window.addEventListener("hashchange", render);
render();
