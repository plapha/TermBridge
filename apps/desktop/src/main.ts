import "./styles.css";
import { commands, invoke, listen, backendAvailable, errorText } from "./ipc";
import { TerminalView } from "./terminal";
import type {
  AttachResult,
  InputRejectedEvent,
  Profile,
  ProfileDraft,
  RemoteSession,
  ResizedEvent,
  SessionClosedEvent,
  SessionInfo,
  SessionOutputEvent,
  SnapshotBeginEvent,
  SnapshotChunkEvent,
  SnapshotEndEvent,
} from "./types";

/* ---------- 状态 ---------- */
const state = {
  profiles: [] as Profile[],
  sessions: [] as SessionInfo[],
  activeSessionId: null as string | null,
  editingProfileId: null as string | null,
};

const terminals = new Map<string, TerminalView>();
const snapshotPending = new Set<string>();
const pendingOutputs = new Map<string, SessionOutputEvent[]>();
const reattaching = new Set<string>();
const unlisteners: Array<() => void> = [];

const $ = <T extends HTMLElement = HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

/* ---------- 真实错误展示（不伪造、不笼统归为“后端未连接”） ---------- */
let toastTimer: number | undefined;

function showError(message: string): void {
  const toast = $("toast");
  toast.textContent = message;
  toast.classList.remove("hidden");
  if (toastTimer !== undefined) clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toast.classList.add("hidden"), 8000);
}

function handleError(err: unknown, prefix: string): void {
  if (err instanceof Error && err.message === "backend_unavailable") {
    showDisconnectedPlaceholder();
    return;
  }
  showError(`${prefix}：${errorText(err)}`);
}

/* ---------- 后端连通性 ---------- */
function setBackendStatus(connected: boolean): void {
  $("backend-status-text").textContent = connected ? "本地核心已连接" : "后端未连接";
  const dot = $("backend-status").querySelector(".dot");
  dot?.classList.toggle("dot-on", connected);
  dot?.classList.toggle("dot-off", !connected);
}

function showDisconnectedPlaceholder(): void {
  setBackendStatus(false);
  const existing = document.querySelector(".backend-disconnected");
  if (existing) return;
  const overlay = document.createElement("div");
  overlay.className = "backend-disconnected";
  overlay.setAttribute("role", "alert");
  overlay.innerHTML = `
    <h2>后端未连接</h2>
    <p>TermBridge 的本地后端（crates/app）尚未运行或不可达。</p>
    <p class="hint">界面处于只读占位状态，所有操作已禁用；不会显示任何伪造的连接结果。</p>`;
  document.getElementById("app")?.appendChild(overlay);
  setActionsEnabled(false);
}

function showBackendRestored(): void {
  document.querySelector(".backend-disconnected")?.remove();
  setBackendStatus(true);
  setActionsEnabled(true);
}

function setActionsEnabled(enabled: boolean): void {
  for (const id of ["btn-new-profile", "btn-send", "host-enable", "host-stop", "host-init"]) {
    const el = document.getElementById(id) as HTMLButtonElement | null;
    if (el) el.disabled = !enabled;
  }
  updateSendButton();
}

function setOfflineBanner(offline: boolean): void {
  $("offline-banner").classList.toggle("hidden", !offline);
}

/* ---------- 事件订阅 ---------- */
async function subscribeEvents(): Promise<void> {
  try {
    unlisteners.push(
      await listen("session_output", (raw) => {
        const ev = raw as { payload?: SessionOutputEvent };
        if (!ev?.payload) return;
        onSessionOutput(ev.payload);
      }),
    );
    unlisteners.push(
      await listen("session_closed", (raw) => {
        const ev = raw as { payload?: SessionClosedEvent };
        if (!ev?.payload) return;
        onSessionClosed(ev.payload);
      }),
    );
    unlisteners.push(
      await listen("session_control_changed", (raw) => {
        const ev = raw as { payload?: { session_id: string; controller: string | null; is_controller: boolean } };
        if (!ev.payload) return;
        const session = state.sessions.find((s) => s.session_id === ev.payload!.session_id);
        if (session) {
          session.controller = ev.payload.controller;
          session.is_controller = ev.payload.is_controller;
          renderTabs(); updateTerminalChrome();
        }
      }),
    );
    unlisteners.push(
      await listen("session_snapshot_begin", (raw) => {
        const ev = raw as { payload?: SnapshotBeginEvent };
        if (!ev?.payload) return;
        onSnapshotBegin(ev.payload);
      }),
    );
    unlisteners.push(
      await listen("session_snapshot_chunk", (raw) => {
        const ev = raw as { payload?: SnapshotChunkEvent };
        if (!ev?.payload) return;
        onSnapshotChunk(ev.payload);
      }),
    );
    unlisteners.push(
      await listen("session_snapshot_end", (raw) => {
        const ev = raw as { payload?: SnapshotEndEvent };
        if (!ev?.payload) return;
        onSnapshotEnd(ev.payload);
      }),
    );
    unlisteners.push(
      await listen("session_resized", (raw) => {
        const ev = raw as { payload?: ResizedEvent };
        if (!ev?.payload) return;
        terminals.get(ev.payload.session_id)?.resizeTo(ev.payload.rows, ev.payload.cols);
      }),
    );
    unlisteners.push(
      await listen("session_input_rejected", (raw) => {
        const ev = raw as { payload?: InputRejectedEvent };
        if (!ev?.payload) return;
        showError(`输入被拒绝（${ev.payload.code}）：${ev.payload.message}`);
      }),
    );
  } catch {
    showDisconnectedPlaceholder();
  }
}

function onSessionOutput(ev: SessionOutputEvent): void {
  setOfflineBanner(false);
  const view = terminals.get(ev.session_id);
  if (!view || snapshotPending.has(ev.session_id)) {
    const queue = pendingOutputs.get(ev.session_id) ?? [];
    queue.push(ev);
    if (queue.length > 2048) queue.splice(0, queue.length - 2048);
    pendingOutputs.set(ev.session_id, queue);
    return;
  }
  if (!view.applyOutput(ev.offset, ev.data_b64)) void resyncSession(ev.session_id);
}

function ensureTerminal(sessionId: string): TerminalView {
  let view = terminals.get(sessionId);
  if (!view) {
    const container = document.createElement("div");
    container.className = "terminal-instance";
    $("terminal-stack").appendChild(container);
    view = new TerminalView(sessionId, container);
    terminals.set(sessionId, view);
  }
  return view;
}

function onSnapshotBegin(ev: SnapshotBeginEvent): void {
  snapshotPending.add(ev.session_id);
  ensureTerminal(ev.session_id).beginSnapshot(ev.offset, ev.rows, ev.cols);
}

function onSnapshotChunk(ev: SnapshotChunkEvent): void {
  terminals.get(ev.session_id)?.snapshotChunk(ev.data_b64);
}

function onSnapshotEnd(ev: SnapshotEndEvent): void {
  terminals.get(ev.session_id)?.endSnapshot();
  snapshotPending.delete(ev.session_id);
  drainBufferedOutput(ev.session_id);
}

function onSessionClosed(ev: SessionClosedEvent): void {
  const s = state.sessions.find((x) => x.session_id === ev.session_id);
  if (s) {
    s.state = ev.reason === "ended" ? "closed" : "error";
    s.online = false;
    s.is_controller = false;
    setOfflineBanner(s.state === "error");
    renderTabs();
    updateTerminalChrome();
  }
}

/**
 * 输出出现缺口：重新 attach。
 * 带 resume_from 时服务端优先从该偏移重放补洞；已越界则回退为快照事件。
 */
async function resyncSession(sessionId: string): Promise<void> {
  if (reattaching.has(sessionId) || snapshotPending.has(sessionId)) return;
  reattaching.add(sessionId);
  setOfflineBanner(true);
  try {
    const view = terminals.get(sessionId);
    const resumeFrom = view?.getOffset() ?? undefined;
    if (view) snapshotPending.add(sessionId);
    const res = await commands.attachSession(sessionId, resumeFrom);
    state.sessions = state.sessions.map((s) =>
      s.session_id === sessionId ? res.session : s,
    );
    if (res.resumed) {
      snapshotPending.delete(sessionId);
      drainBufferedOutput(sessionId);
    }
    setOfflineBanner(false);
    renderTabs();
    updateTerminalChrome();
  } catch (err) {
    snapshotPending.delete(sessionId);
    handleError(err, "画面重新同步失败");
  } finally {
    reattaching.delete(sessionId);
  }
}

/* ---------- 连接配置 ---------- */
async function refreshProfiles(): Promise<void> {
  try {
    state.profiles = await commands.listProfiles();
    showBackendRestored();
    renderProfiles();
  } catch (err) {
    handleError(err, "读取连接配置失败");
  }
}

function renderProfiles(): void {
  const list = $("profile-list");
  list.textContent = "";
  if (state.profiles.length === 0) {
    const li = document.createElement("li");
    li.className = "hint";
    li.textContent = "暂无保存的连接配置。";
    list.appendChild(li);
    return;
  }
  for (const p of state.profiles) {
    const li = document.createElement("li");
    li.className = "profile-item";
    li.innerHTML = `
      <div class="profile-title"></div>
      <div class="profile-meta mono"></div>
      <div class="profile-actions">
        <button type="button" data-act="connect">新建终端</button>
        <button type="button" data-act="existing">已有终端</button>
        <button type="button" data-act="edit">编辑</button>
        <button type="button" data-act="remove">删除</button>
      </div>`;
    li.querySelector(".profile-title")!.textContent = p.name;
    li.querySelector(".profile-meta")!.textContent = `${p.user}@${p.host}:${p.port}`;
    li.querySelectorAll("button").forEach((btn) => {
      const act = btn.getAttribute("data-act");
      if (act === "connect") btn.addEventListener("click", () => void connectProfile(p));
      if (act === "existing") btn.addEventListener("click", () => void connectProfile(p, true));
      if (act === "edit") btn.addEventListener("click", () => openProfileForm(p));
      if (act === "remove") btn.addEventListener("click", () => void removeProfile(p));
    });
    list.appendChild(li);
  }
}

function openProfileForm(p?: Profile): void {
  state.editingProfileId = p?.id ?? null;
  $("profile-form").classList.remove("hidden");
  $("profile-form-title").textContent = p ? "编辑连接配置" : "新建连接配置";
  ($("pf-name") as HTMLInputElement).value = p?.name ?? "";
  ($("pf-host") as HTMLInputElement).value = p?.host ?? "";
  ($("pf-port") as HTMLInputElement).value = String(p?.port ?? 22333);
  ($("pf-user") as HTMLInputElement).value = p?.user ?? "";
  const auth = p?.auth ?? "key";
  (document.querySelector(`input[name="auth"][value="${auth}"]`) as HTMLInputElement).checked = true;
  ($("pf-key-path") as HTMLInputElement).value = p?.key_path ?? "";
  $("pf-key-row").classList.toggle("hidden", auth !== "key");
}

function closeProfileForm(): void {
  state.editingProfileId = null;
  $("profile-form").classList.add("hidden");
}

async function saveProfileForm(): Promise<void> {
  const draft: ProfileDraft = {
    id: state.editingProfileId ?? undefined,
    name: ($("pf-name") as HTMLInputElement).value.trim(),
    host: ($("pf-host") as HTMLInputElement).value.trim(),
    port: Number(($("pf-port") as HTMLInputElement).value) || 22333,
    user: ($("pf-user") as HTMLInputElement).value.trim(),
    auth: (document.querySelector('input[name="auth"]:checked') as HTMLInputElement)
      .value as ProfileDraft["auth"],
    key_path: ($("pf-key-path") as HTMLInputElement).value.trim() || null,
  };
  try {
    await commands.saveProfile(draft);
    closeProfileForm();
    await refreshProfiles();
  } catch (err) {
    handleError(err, "保存连接配置失败");
  }
}

async function removeProfile(p: Profile): Promise<void> {
  try {
    await commands.removeProfile(p.id);
    await refreshProfiles();
  } catch (err) {
    handleError(err, "删除连接配置失败");
  }
}

/* ---------- 指纹确认（显式，绝不 TOFU） ---------- */
function confirmFingerprintModal(host: string, port: number, fingerprint: string): Promise<boolean> {
  return new Promise((resolve) => {
    const modal = $("fp-modal");
    $("fp-modal-host").textContent = `${host}:${port}`;
    $("fp-modal-value").textContent = fingerprint;
    const done = (ok: boolean) => {
      modal.classList.add("hidden");
      $("fp-confirm").removeEventListener("click", onConfirm);
      $("fp-cancel").removeEventListener("click", onCancel);
      resolve(ok);
    };
    const onConfirm = () => done(true);
    const onCancel = () => done(false);
    $("fp-confirm").addEventListener("click", onConfirm);
    $("fp-cancel").addEventListener("click", onCancel);
    modal.classList.remove("hidden");
  });
}

/* ---------- 连接密码（本次输入 / 记住到凭据库） ---------- */
function passwordModal(host: string, port: number, kind: "password" | "key" = "password"): Promise<{ password: string; remember: boolean } | null> {
  return new Promise((resolve) => {
    const modal = $("pw-modal");
    $("pw-modal-host").textContent = `${host}:${port}`;
    $("pw-modal-title").textContent = kind === "key" ? "SSH 私钥口令" : "连接密码";
    $("pw-modal-label").textContent = kind === "key" ? "私钥口令" : "密码";
    $("pw-modal-hint").textContent = kind === "key" ? "口令只用于本次解密 SSH 私钥，不会保存或记录。" : "不勾选时密码仅用于本次连接，不会被保存或记录。";
    $("pw-remember-row").classList.toggle("hidden", kind === "key");
    const input = $("pw-input") as HTMLInputElement;
    const remember = $("pw-remember") as HTMLInputElement;
    input.value = "";
    remember.checked = false;
    const done = (result: { password: string; remember: boolean } | null) => {
      modal.classList.add("hidden");
      input.value = "";
      $("pw-ok").removeEventListener("click", onOk);
      $("pw-cancel").removeEventListener("click", onCancel);
      resolve(result);
    };
    const onOk = () => {
      const password = input.value;
      if (!password) {
        showError(kind === "key" ? "请输入 SSH 私钥口令" : "缺少连接密码：请输入密码后再连接");
        return;
      }
      done({ password, remember: kind === "password" && remember.checked });
    };
    const onCancel = () => done(null);
    $("pw-ok").addEventListener("click", onOk);
    $("pw-cancel").addEventListener("click", onCancel);
    modal.classList.remove("hidden");
    input.focus();
  });
}

/* ---------- 会话 ---------- */
function chooseExistingSession(sessions: RemoteSession[]): Promise<string | null> {
  return new Promise((resolve) => {
    const modal = $("existing-modal");
    const select = $("existing-session-list") as HTMLSelectElement;
    select.textContent = "";
    for (const s of sessions.filter((s) => s.live)) {
      const item = document.createElement("option");
      item.value = s.id;
      item.textContent = `${s.title} · ${s.id}`;
      select.append(item);
    }
    if (!select.options.length) { showError("没有仍在运行的会话；设备重启后旧终端不会自动重建"); resolve(null); return; }
    const done = (value: string | null) => {
      modal.classList.add("hidden");
      $("existing-ok").removeEventListener("click", onOk);
      $("existing-cancel").removeEventListener("click", onCancel);
      resolve(value);
    };
    const onOk = () => done(select.value);
    const onCancel = () => done(null);
    $("existing-ok").addEventListener("click", onOk);
    $("existing-cancel").addEventListener("click", onCancel);
    modal.classList.remove("hidden");
  });
}

async function rememberPasswordIfAvailable(profileId: string, password: string): Promise<void> {
  try {
    await commands.storeProfilePassword(profileId, password);
    await refreshProfiles();
  } catch (err) {
    showError(`本次连接继续，但系统凭据库无法保存密码：${errorText(err)}`);
  }
}
async function connectProfile(p: Profile, existing = false): Promise<void> {
  try {
    const probed = await commands.probeHost(p.host, p.port);
    if (!probed.trusted) {
      const ok = await confirmFingerprintModal(p.host, p.port, probed.fingerprint);
      if (!ok) return;
      await commands.confirmHostFingerprint(p.host, p.port, probed.fingerprint);
    }
    let password: string | undefined;
    let rememberNewPassword = false;
    if (p.auth === "password") {
      const useSaved = p.remember_password && window.confirm("使用系统凭据库中已记住的密码？取消则重新输入。");
      if (!useSaved) {
        const entered = await passwordModal(p.host, p.port);
        if (!entered) return;
        password = entered.password;
        rememberNewPassword = entered.remember;
      }
    } else if (await commands.keyPassphraseRequired(p.id)) {
      const entered = await passwordModal(p.host, p.port, "key");
      if (!entered) return;
      password = entered.password;
    }
    if (existing) {
      const sessions = await commands.listRemoteSessions(p.id, password);
      if (rememberNewPassword && password) await rememberPasswordIfAvailable(p.id, password);
      const id = await chooseExistingSession(sessions);
      if (!id) return;
      snapshotPending.add(id);
      let res;
      try { res = await commands.connectExistingSession(p.id, id, password); }
      catch (err) { snapshotPending.delete(id); throw err; }
      state.sessions = state.sessions.filter((s) => s.session_id !== id);
      state.sessions.push(res.session);
      ensureTerminal(id);
      state.activeSessionId = id;
      activateTerminal(id);
      renderTabs();
      updateTerminalChrome();
    } else {
      const { session } = await commands.createSession(p.id, password);
      if (rememberNewPassword && password) await rememberPasswordIfAvailable(p.id, password);
      state.sessions.push(session);
      await attachSession(session.session_id);
    }
  } catch (err) {
    handleError(err, "连接失败");
  }
}

function registerAttachedSession(sessionId: string, res: AttachResult): void {
  state.sessions = state.sessions.map((s) => s.session_id === sessionId ? res.session : s);
  ensureTerminal(sessionId);
  if (res.resumed) {
    // 服务端补发而不是快照：清掉等待标记，事件队列里的输出马上可用。
    snapshotPending.delete(sessionId);
    drainBufferedOutput(sessionId);
  }
  state.activeSessionId = sessionId;
  activateTerminal(sessionId);
  renderTabs();
  updateTerminalChrome();
}

async function attachSession(sessionId: string): Promise<void> {
  snapshotPending.add(sessionId);
  try {
    const res = await commands.attachSession(sessionId);
    registerAttachedSession(sessionId, res);
  } catch (err) {
    snapshotPending.delete(sessionId);
    handleError(err, "附加会话失败");
  }
}

function drainBufferedOutput(sessionId: string): void {
  const queue = pendingOutputs.get(sessionId) ?? [];
  pendingOutputs.delete(sessionId);
  const view = terminals.get(sessionId);
  if (!view) return;
  for (const ev of queue) {
    if (!view.applyOutput(ev.offset, ev.data_b64)) {
      void resyncSession(sessionId);
      break;
    }
  }
}

function activateTerminal(sessionId: string): void {
  const stack = $("terminal-stack");
  stack.classList.add("active");
  for (const [id, view] of terminals) {
    (view["container"] as HTMLElement).style.display =
      id === sessionId ? "block" : "none";
    if (id === sessionId) view.fit();
  }
  $("terminal-placeholder").classList.add("hidden");
}

function renderTabs(): void {
  const nav = $("session-tabs");
  nav.querySelectorAll(".tab").forEach((t) => t.remove());
  $("tabs-empty").classList.toggle("hidden", state.sessions.length > 0);
  for (const s of state.sessions) {
    const tab = document.createElement("div");
    tab.className = "tab";
    tab.setAttribute("role", "tab");
    tab.setAttribute("tabindex", "0");
    tab.setAttribute(
      "aria-selected",
      String(s.session_id === state.activeSessionId),
    );
    const label = document.createElement("span");
    label.textContent = s.profile_name;
    const st = document.createElement("span");
    st.className = `tab-state ${s.state}`;
    st.textContent = stateLabel(s.state);
    const role = document.createElement("span");
    role.className = "role";
    role.textContent = s.is_controller ? "控制器" : "只读";
    const detach = document.createElement("button");
    detach.type = "button";
    detach.textContent = "分离";
    detach.title = "分离：保留会话，仅停止查看";
    detach.addEventListener("click", (e) => {
      e.stopPropagation();
      void detachCurrent(s.session_id);
    });
    const kill = document.createElement("button");
    kill.type = "button";
    kill.textContent = "终止";
    kill.title = "终止：结束该会话";
    kill.addEventListener("click", (e) => {
      e.stopPropagation();
      void endSession(s.session_id);
    });
    const take = document.createElement("button");
    take.type = "button";
    take.textContent = "接管输入";
    take.disabled = s.is_controller || !s.online || s.state !== "attached";
    take.addEventListener("click", (e) => {
      e.stopPropagation();
      void commands.takeControl(s.session_id).then((res) => {
        state.sessions = state.sessions.map((x) => x.session_id === s.session_id ? res.session : x);
        renderTabs(); updateTerminalChrome();
      }).catch((err) => handleError(err, "接管输入失败"));
    });
    tab.append(label, st, role, take, detach, kill);
    tab.addEventListener("click", () => {
      state.activeSessionId = s.session_id;
      activateTerminal(s.session_id);
      renderTabs();
      updateTerminalChrome();
    });
    nav.appendChild(tab);
  }
  updateTerminalChrome();
}

function stateLabel(s: SessionInfo["state"]): string {
  switch (s) {
    case "connecting":
      return "连接中";
    case "attached":
      return "已附加";
    case "detached":
      return "已分离";
    case "closed":
      return "已结束";
    case "error":
      return "错误";
  }
}

async function detachCurrent(sessionId: string): Promise<void> {
  try {
    const s = await commands.detachSession(sessionId);
    state.sessions = state.sessions.map((x) => (x.session_id === s.session_id ? s : x));
    if (state.activeSessionId === sessionId) {
      state.activeSessionId = null;
      $("terminal-stack").classList.remove("active");
      $("terminal-placeholder").classList.remove("hidden");
    }
    renderTabs();
    updateSendButton();
  } catch (err) {
    handleError(err, "分离会话失败");
  }
}

async function endSession(sessionId: string): Promise<void> {
  if (!window.confirm("确定结束远端终端进程？分离可保留会话，结束后无法恢复。")) return;
  try {
    const s = await commands.endSession(sessionId);
    state.sessions = state.sessions.map((x) => (x.session_id === s.session_id ? s : x));
    terminals.get(sessionId)?.dispose();
    terminals.delete(sessionId);
    if (state.activeSessionId === sessionId) {
      state.activeSessionId = null;
      $("terminal-stack").classList.remove("active");
      $("terminal-placeholder").classList.remove("hidden");
    }
    renderTabs();
    updateSendButton();
  } catch (err) {
    handleError(err, "结束会话失败");
  }
}

function currentSession(): SessionInfo | null {
  return state.sessions.find((s) => s.session_id === state.activeSessionId) ?? null;
}

/** 控制器 / 只读 / 离线状态展示与发送按钮可用性。 */
function updateTerminalChrome(): void {
  const s = currentSession();
  const target = $("composer-target");
  if (!s) {
    target.textContent = "未选择会话";
  } else {
    const role = s.is_controller ? "控制器" : "只读";
    target.textContent = `${s.profile_name} · ${role}${s.online ? "" : " · 离线，请从“已有终端”重新连接"}`;
  }
  updateSendButton();
}

function updateSendButton(): void {
  const s = currentSession();
  const btn = $("btn-send") as HTMLButtonElement;
  btn.disabled = !backendAvailable || !s || !s.is_controller || !s.online;
}

/* ---------- 发送 ---------- */
async function sendComposer(): Promise<void> {
  const s = currentSession();
  if (!s || !s.is_controller) return;
  const input = $("composer-input") as HTMLInputElement;
  const text = input.value;
  if (!text) return;
  try {
    // v2：传输原始终端字节（Enter = CR），偏移由后端维护。
    const bytes = new TextEncoder().encode(`${text}\r`);
    await commands.sendInput(s.session_id, Array.from(bytes));
    input.value = "";
  } catch (err) {
    handleError(err, "发送失败");
  }
}

/* ---------- 主机面板 ---------- */
function composeBindAddr(): string {
  const select = $("host-bind-select") as HTMLSelectElement;
  const custom = ($("host-bind-custom") as HTMLInputElement).value.trim();
  const ip = select.value === "custom" ? custom : select.value;
  const port = Number(($("host-port") as HTMLInputElement).value);
  if (!ip) return "";
  const address = ip.includes(":") && !ip.startsWith("[") ? `[${ip}]` : ip;
  return `${address}:${Number.isFinite(port) && port > 0 ? port : 22333}`;
}

async function refreshHost(): Promise<void> {
  try {
    const st = await commands.hostStatus();
    showBackendRestored();
    const badge = $("host-state-badge");
    badge.textContent = st.enabled ? "运行中" : "已停止";
    badge.className = `badge ${st.enabled ? "badge-running" : "badge-stopped"}`;
    $("host-status-text").textContent = st.enabled ? "运行中" : "已停止";
    $("host-fingerprint").textContent = st.fingerprint ?? "—";
    $("host-auth-status").textContent = !st.fingerprint ? "—" : st.password_enabled
      ? `产品密码${st.authorized_key_count ? ` + ${st.authorized_key_count} 把公钥` : ""}`
      : `仅 SSH 密钥（${st.authorized_key_count} 把）`;
    $("host-controller").textContent = st.controller ?? "—";
    $("host-migrate-section").classList.toggle("hidden", !st.fingerprint || !st.password_enabled);
    ($("host-migrate-keys") as HTMLButtonElement).disabled = st.enabled;
    // 未初始化（无指纹）时展示初始化区；已初始化则隐藏。
    $("host-init-section").classList.toggle("hidden", !!st.fingerprint);
    if (st.bind_addr) {
      const idx = st.bind_addr.lastIndexOf(":");
      const ip = idx >= 0 ? st.bind_addr.slice(0, idx) : st.bind_addr;
      const port = idx >= 0 ? st.bind_addr.slice(idx + 1) : "";
      const select = $("host-bind-select") as HTMLSelectElement;
      select.value = ip;
      if (select.selectedIndex < 0) {
        select.value = "custom";
        ($("host-bind-custom-row") as HTMLElement).classList.remove("hidden");
        ($("host-bind-custom") as HTMLInputElement).value = ip;
      } else {
        ($("host-bind-custom-row") as HTMLElement).classList.add("hidden");
      }
      if (port && /^\d+$/.test(port)) ($("host-port") as HTMLInputElement).value = port;
    }
    ($("host-stop") as HTMLButtonElement).disabled = !st.enabled;
    ($("host-enable") as HTMLButtonElement).disabled = !st.fingerprint;
  } catch (err) {
    handleError(err, "读取接收状态失败");
  }
}

async function initHost(): Promise<void> {
  const useKeys = (document.querySelector('input[name="host-auth"]:checked') as HTMLInputElement).value === "key";
  const password = ($("host-init-password") as HTMLInputElement).value;
  if (!useKeys && password.length < 12) {
    showError("接收密码至少需要 12 个字符");
    return;
  }
  try {
    const st = useKeys
      ? await commands.initHost(undefined, ($("host-init-key-path") as HTMLInputElement).value.trim())
      : await commands.initHost(password);
    ($("host-init-password") as HTMLInputElement).value = "";
    $("host-init-section").classList.add("hidden");
    $("host-fingerprint").textContent = st.fingerprint ?? "—";
    ($("host-enable") as HTMLButtonElement).disabled = false;
    showBackendRestored();
  } catch (err) {
    handleError(err, "初始化接收端失败");
  }
}

async function switchHostToKeys(): Promise<void> {
  if (!window.confirm("确定停用产品专用密码，改为只接受所选文件中的 SSH 授权公钥？请先确保你持有对应私钥。")) return;
  try {
    await commands.switchHostToKeys(($("host-migrate-key-path") as HTMLInputElement).value.trim());
    await refreshHost();
  } catch (err) {
    handleError(err, "切换密钥认证失败");
  }
}

async function setHostEnabled(enabled: boolean): Promise<void> {
  const bind = composeBindAddr();
  if (enabled && !bind.includes(":")) {
    showError("启用接收端必须提供完整监听地址（IP:端口，如 127.0.0.1:22333）");
    return;
  }
  try {
    await commands.setHostEnabled(enabled, bind);
    await refreshHost();
  } catch (err) {
    handleError(err, enabled ? "启用接收端失败" : "停止接收端失败");
  }
}

/* ---------- 启动 ---------- */
async function pollBackend(): Promise<void> {
  // 简单重连探测：后端恢复后自动重新加载。
  try {
    state.sessions = await commands.listSessions();
    showBackendRestored();
    renderTabs();
    setOfflineBanner(state.sessions.some((s) => s.state === "error"));
    await Promise.all([refreshProfiles(), refreshHost()]);
  } catch (err) {
    if (err instanceof Error && err.message === "backend_unavailable") {
      setOfflineBanner(true);
    }
  }
}

function wireUi(): void {
  $("btn-new-profile").addEventListener("click", () => openProfileForm());
  $("btn-hide-to-tray").addEventListener("click", () => {
    void invoke("hide_to_tray").catch((err) => handleError(err, "隐藏到托盘失败"));
  });
  $("pf-cancel").addEventListener("click", closeProfileForm);
  $("profile-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void saveProfileForm();
  });
  $("btn-send").addEventListener("click", () => void sendComposer());
  $("host-enable").addEventListener("click", () => void setHostEnabled(true));
  $("host-stop").addEventListener("click", () => void setHostEnabled(false));
  $("host-init").addEventListener("click", () => void initHost());
  $("host-migrate-keys").addEventListener("click", () => void switchHostToKeys());
  document.querySelectorAll('input[name="host-auth"]').forEach((radio) => radio.addEventListener("change", () => {
    const useKeys = (document.querySelector('input[name="host-auth"]:checked') as HTMLInputElement).value === "key";
    $("host-init-key-row").classList.toggle("hidden", !useKeys);
    $("host-init-password-row").classList.toggle("hidden", useKeys);
  }));
  document.querySelectorAll('input[name="auth"]').forEach((radio) => radio.addEventListener("change", () => {
    const selected = (document.querySelector('input[name="auth"]:checked') as HTMLInputElement).value;
    $("pf-key-row").classList.toggle("hidden", selected !== "key");
  }));
  ($("host-bind-select") as HTMLSelectElement).addEventListener("change", () => {
    const select = $("host-bind-select") as HTMLSelectElement;
    $("host-bind-custom-row").classList.toggle("hidden", select.value !== "custom");
  });
  window.addEventListener("resize", () => {
    terminals.get(state.activeSessionId ?? "")?.fit();
  });
}

async function boot(): Promise<void> {
  wireUi();
  await subscribeEvents();
  await Promise.all([refreshProfiles(), refreshHost()]);
  if (!backendAvailable) showDisconnectedPlaceholder();
  // 后端可能尚未启动：周期性探测，恢复后自动加载（真实状态，不伪造）。
  setInterval(() => void pollBackend(), 5000);
}

void boot();
