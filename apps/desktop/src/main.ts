import "./styles.css";
import { commands, invoke, listen, backendAvailable, errorText } from "./ipc";
import { TerminalView, measureTerminalSize } from "./terminal";
import { applyStatic, getLang, onLangChange, parseLang, setLang, t } from "./i18n";
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
const MAX_INPUT_INVOKE = 16 * 1024;
const MAX_PENDING_INPUT = 256 * 1024;
interface InputQueue { pending: number[]; sending: boolean; closed: boolean }
const inputQueues = new Map<string, InputQueue>();

/** JS 单线程按 onData/onBinary 到达顺序入队；只有一个循环可调用异步 invoke。 */
function enqueueInput(sessionId: string, bytes: Uint8Array): void {
  let queue = inputQueues.get(sessionId);
  if (!queue) {
    queue = { pending: [], sending: false, closed: false };
    inputQueues.set(sessionId, queue);
  }
  if (queue.pending.length + bytes.length > MAX_PENDING_INPUT) {
    showHint(t("hint.queueFull") + "\x07");
    return;
  }
  for (const byte of bytes) queue.pending.push(byte);
  if (queue.sending) return;
  queue.sending = true;
  void drainInput(sessionId, queue);
}

async function drainInput(sessionId: string, queue: InputQueue): Promise<void> {
  try {
    while (!queue.closed && queue.pending.length) {
      const batch = queue.pending.splice(0, MAX_INPUT_INVOKE);
      // 一个 invoke 完成之后才发送下一个；失败时不猜测状态也不自动重发。
      await commands.sendInput(sessionId, batch);
    }
  } catch (err) {
    queue.pending.length = 0;
    handleError(err, t("error.inputSend"));
  } finally {
    queue.sending = false;
    if (!queue.closed && queue.pending.length) {
      queue.sending = true;
      void drainInput(sessionId, queue);
    }
  }
}

function closeInputQueue(sessionId: string): void {
  const queue = inputQueues.get(sessionId);
  if (queue) { queue.closed = true; queue.pending.length = 0; }
  inputQueues.delete(sessionId);
}
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
  showError(t("error.withPrefix", { prefix, detail: errorText(err) }));
}

/* ---------- 后端连通性 ---------- */
let backendConnected = false;

function setBackendStatus(connected: boolean): void {
  backendConnected = connected;
  $("backend-status-text").textContent = connected ? t("backend.connected") : t("backend.disconnected");
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
  fillDisconnectedOverlay(overlay);
  document.getElementById("app")?.appendChild(overlay);
  setActionsEnabled(false);
}

function fillDisconnectedOverlay(overlay: HTMLElement): void {
  overlay.innerHTML = `
    <h2>${t("backend.disconnected")}</h2>
    <p>${t("backend.unreachable")}</p>
    <p class="hint">${t("backend.readonlyHint")}</p>`;
}

function showBackendRestored(): void {
  document.querySelector(".backend-disconnected")?.remove();
  setBackendStatus(true);
  setActionsEnabled(true);
}

function setActionsEnabled(enabled: boolean): void {
  for (const id of ["btn-new-profile", "host-enable", "host-stop", "host-init"]) {
    const el = document.getElementById(id) as HTMLButtonElement | null;
    if (el) el.disabled = !enabled;
  }
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
          syncControllerState(session.session_id);
          if (session.is_controller) {
            const view = terminals.get(session.session_id);
            view?.fit();
            void syncResize(session.session_id);
          }
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
        showError(t("error.inputRejected", { code: ev.payload.code, message: ev.payload.message }));
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
    view = new TerminalView(sessionId, container, {
      onInput: (bytes) => enqueueInput(sessionId, bytes),
      onBlockedInput: () => showHint(t("hint.observer")),
      onResize: () => void syncResize(sessionId),
      onClipboardError: () => showHint(t("hint.clipboard")),
    });
    terminals.set(sessionId, view);
  }
  return view;
}

let hintTimer: number | undefined;

function showHint(message: string): void {
  const hint = $("terminal-hint");
  hint.textContent = message;
  hint.classList.remove("hidden");
  if (hintTimer !== undefined) window.clearTimeout(hintTimer);
  hintTimer = window.setTimeout(() => hint.classList.add("hidden"), 4000);
}

/** 只有控制者且在线时可输入；其他情况在终端层拦下按键。 */
function syncControllerState(sessionId: string): void {
  const s = state.sessions.find((x) => x.session_id === sessionId);
  const view = terminals.get(sessionId);
  if (!s || !view) return;
  const canInput =
    s.is_controller && s.online && s.state !== "closed" && s.state !== "error";
  view.setController(canInput);
}

/** 把当前 fit 尺寸发给远端（仅控制者）。 */
async function syncResize(sessionId: string): Promise<void> {
  const s = state.sessions.find((x) => x.session_id === sessionId);
  const view = terminals.get(sessionId);
  if (!s?.is_controller || !s.online || !view) return;
  const { rows, cols } = view.getSize();
  if (rows < 2 || cols < 2) return;
  try {
    await commands.resizeSession(sessionId, rows, cols);
  } catch (err) {
    handleError(err, t("error.resize"));
  }
}

function onSnapshotBegin(ev: SnapshotBeginEvent): void {
  snapshotPending.add(ev.session_id);
  ensureTerminal(ev.session_id).beginSnapshot(ev.offset, ev.rows, ev.cols);
}

function onSnapshotChunk(ev: SnapshotChunkEvent): void {
  terminals.get(ev.session_id)?.snapshotChunk(ev.data_b64);
}

function onSnapshotEnd(ev: SnapshotEndEvent): void {
  const view = terminals.get(ev.session_id);
  const offset = view?.endSnapshot();
  snapshotPending.delete(ev.session_id);
  if (view && offset === null) {
    // 本地积压超限：丢弃缓冲并重新挂接拿快照。
    void resyncSession(ev.session_id);
    return;
  }
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
    handleError(err, t("error.resync"));
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
    handleError(err, t("error.loadProfiles"));
  }
}

function renderProfiles(): void {
  const list = $("profile-list");
  list.textContent = "";
  if (state.profiles.length === 0) {
    const li = document.createElement("li");
    li.className = "hint";
    li.textContent = t("profiles.empty");
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
        <button type="button" data-act="connect">${t("profile.newTerminal")}</button>
        <button type="button" data-act="existing">${t("profile.existing")}</button>
        <button type="button" data-act="edit">${t("profile.edit")}</button>
        <button type="button" data-act="remove">${t("profile.remove")}</button>
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
  $("profile-form-title").textContent = p ? t("profiles.formEdit") : t("profiles.formNew");
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
    handleError(err, t("error.saveProfile"));
  }
}

async function removeProfile(p: Profile): Promise<void> {
  try {
    await commands.removeProfile(p.id);
    await refreshProfiles();
  } catch (err) {
    handleError(err, t("error.removeProfile"));
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
    $("pw-modal-title").textContent = kind === "key" ? t("pw.titleKey") : t("pw.titlePassword");
    $("pw-modal-label").textContent = kind === "key" ? t("pw.labelKey") : t("pw.labelPassword");
    $("pw-modal-hint").textContent = kind === "key" ? t("pw.hintKey") : t("pw.hintPassword");
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
        showError(kind === "key" ? t("pw.needKey") : t("pw.needPassword"));
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
    if (!select.options.length) { showError(t("existing.none")); resolve(null); return; }
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
    showError(t("pw.rememberFailed", { detail: errorText(err) }));
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
      const useSaved = p.remember_password && window.confirm(t("pw.confirmUseSaved"));
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
      // 用与终端相同字体测量实际可用行列数，不再写死 24x80。
      const { rows, cols } = measureTerminalSize($("terminal-panel"));
      const { session } = await commands.createSession(p.id, password, rows, cols);
      if (rememberNewPassword && password) await rememberPasswordIfAvailable(p.id, password);
      state.sessions.push(session);
      await attachSession(session.session_id);
    }
  } catch (err) {
    handleError(err, t("error.connect"));
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
  // 获得控制权后立即发送一次实际 fit 尺寸。
  const view = terminals.get(sessionId);
  if (view && res.session.is_controller) {
    view.fit();
    void syncResize(sessionId);
  }
  syncControllerState(sessionId);
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
    handleError(err, t("error.attach"));
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
    role.textContent = s.is_controller ? t("tab.controller") : t("tab.readonly");
    const detach = document.createElement("button");
    detach.type = "button";
    detach.textContent = t("tab.detach");
    detach.title = t("tab.detachTitle");
    detach.addEventListener("click", (e) => {
      e.stopPropagation();
      void detachCurrent(s.session_id);
    });
    const kill = document.createElement("button");
    kill.type = "button";
    kill.textContent = t("tab.terminate");
    kill.title = t("tab.terminateTitle");
    kill.addEventListener("click", (e) => {
      e.stopPropagation();
      void endSession(s.session_id);
    });
    const take = document.createElement("button");
    take.type = "button";
    take.textContent = t("tab.takeControl");
    take.disabled = s.is_controller || !s.online || s.state !== "attached";
    take.addEventListener("click", (e) => {
      e.stopPropagation();
      void commands.takeControl(s.session_id).then((res) => {
        state.sessions = state.sessions.map((x) => x.session_id === s.session_id ? res.session : x);
        const view = terminals.get(s.session_id);
        syncControllerState(s.session_id);
        view?.fit();
        void syncResize(s.session_id);
        renderTabs(); updateTerminalChrome();
      }).catch((err) => handleError(err, t("error.takeControl")));
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
      return t("state.connecting");
    case "attached":
      return t("state.attached");
    case "detached":
      return t("state.detached");
    case "closed":
      return t("state.closed");
    case "error":
      return t("state.error");
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
    syncControllerState(sessionId);
    renderTabs();
  } catch (err) {
    handleError(err, t("error.detach"));
  }
}

async function endSession(sessionId: string): Promise<void> {
  if (!window.confirm(t("confirm.end"))) return;
  try {
    const s = await commands.endSession(sessionId);
    state.sessions = state.sessions.map((x) => (x.session_id === s.session_id ? s : x));
    closeInputQueue(sessionId);
    terminals.get(sessionId)?.dispose();
    terminals.delete(sessionId);
    if (state.activeSessionId === sessionId) {
      state.activeSessionId = null;
      $("terminal-stack").classList.remove("active");
      $("terminal-placeholder").classList.remove("hidden");
    }
    renderTabs();
  } catch (err) {
    handleError(err, t("error.end"));
  }
}

function currentSession(): SessionInfo | null {
  return state.sessions.find((s) => s.session_id === state.activeSessionId) ?? null;
}

/** 控制器 / 只读 / 离线状态展示；输入可用性按会话状态同步到各终端。 */
function updateTerminalChrome(): void {
  const s = currentSession();
  for (const id of terminals.keys()) syncControllerState(id);
  if (s && !s.online) showHint(t("hint.offline"));
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
    badge.textContent = st.enabled ? t("host.running") : t("host.stopped");
    badge.className = `badge ${st.enabled ? "badge-running" : "badge-stopped"}`;
    $("host-status-text").textContent = st.enabled ? t("host.running") : t("host.stopped");
    $("host-fingerprint").textContent = st.fingerprint ?? "—";
    $("host-auth-status").textContent = !st.fingerprint ? "—" : st.password_enabled
      ? (st.authorized_key_count
          ? t("host.authStatusPasswordKeys", { count: st.authorized_key_count })
          : t("host.authStatusPassword"))
      : t("host.authStatusKeys", { count: st.authorized_key_count });
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
    handleError(err, t("error.hostStatus"));
  }
}

async function initHost(): Promise<void> {
  const useKeys = (document.querySelector('input[name="host-auth"]:checked') as HTMLInputElement).value === "key";
  const password = ($("host-init-password") as HTMLInputElement).value;
  if (!useKeys && password.length < 12) {
    showError(t("host.passwordShort"));
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
    handleError(err, t("error.initHost"));
  }
}

async function switchHostToKeys(): Promise<void> {
  if (!window.confirm(t("host.confirmMigrate"))) return;
  try {
    await commands.switchHostToKeys(($("host-migrate-key-path") as HTMLInputElement).value.trim());
    await refreshHost();
  } catch (err) {
    handleError(err, t("error.switchKeys"));
  }
}

async function setHostEnabled(enabled: boolean): Promise<void> {
  const bind = composeBindAddr();
  if (enabled && !bind.includes(":")) {
    showError(t("host.needAddress"));
    return;
  }
  try {
    await commands.setHostEnabled(enabled, bind);
    await refreshHost();
  } catch (err) {
    handleError(err, enabled ? t("error.enableHost") : t("error.stopHost"));
  }
}

/* ---------- 界面语言 ---------- */
function pushLanguage(): Promise<void> {
  // 后端不可达时忽略：恢复连接后的下一次切换或启动会再同步。
  return commands.setLanguage(getLang()).catch(() => {});
}

/** 语言切换后重绘所有动态生成的文案（静态文案由 applyStatic 处理）。 */
function renderLanguage(): void {
  setBackendStatus(backendConnected);
  const overlay = document.querySelector<HTMLElement>(".backend-disconnected");
  if (overlay) fillDisconnectedOverlay(overlay);
  renderProfiles();
  renderTabs();
  if (!$("profile-form").classList.contains("hidden")) {
    $("profile-form-title").textContent = state.editingProfileId ? t("profiles.formEdit") : t("profiles.formNew");
  }
  void refreshHost();
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
  const langSelect = $("lang-select") as HTMLSelectElement;
  langSelect.value = getLang();
  langSelect.addEventListener("change", () => {
    const lang = parseLang(langSelect.value);
    if (lang) setLang(lang);
  });
  onLangChange(() => {
    renderLanguage();
    void pushLanguage();
  });
  $("btn-new-profile").addEventListener("click", () => openProfileForm());
  $("btn-hide-to-tray").addEventListener("click", () => {
    void invoke("hide_to_tray").catch((err) => handleError(err, t("error.hideToTray")));
  });
  $("pf-cancel").addEventListener("click", closeProfileForm);
  $("profile-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void saveProfileForm();
  });

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
  applyStatic();
  // 动态文案的初始值：后端状态、主机状态在首次刷新前也要用当前语言。
  setBackendStatus(false);
  $("host-state-badge").textContent = t("host.stopped");
  $("host-status-text").textContent = t("host.unknown");
  wireUi();
  void pushLanguage();
  await subscribeEvents();
  await Promise.all([refreshProfiles(), refreshHost()]);
  if (!backendAvailable) showDisconnectedPlaceholder();
  // 后端可能尚未启动：周期性探测，恢复后自动加载（真实状态，不伪造）。
  setInterval(() => void pollBackend(), 5000);
}

void boot();
