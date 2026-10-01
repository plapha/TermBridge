import type {
  AttachResult,
  CreateSessionResult,
  FingerprintResult,
  HostStatus,
  ListenFn,
  Profile,
  ProfileDraft,
  SessionInfo,
  TakeControlResult,
  UnlistenFn,
  RemoteSession,
} from "./types";

/**
 * 对 Tauri invoke / listen 的薄封装。
 *
 * 错误契约：
 * - 仅当 Tauri 运行时 / 本地后端不可达时抛出 Error("backend_unavailable")；
 * - 命令本身的失败（后端返回 IpcError）原样向上抛出，
 *   由 UI 层展示真实错误信息，绝不笼统归为“后端未连接”，
 *   也绝不伪造成功结果。
 */

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type TauriGlobal = { invoke?: any; listen?: any };

function tauri(): TauriGlobal | null {
  const w = window as unknown as { __TAURI_INTERNALS__?: unknown; __TAURI__?: TauriGlobal };
  if (w.__TAURI_INTERNALS__ || w.__TAURI__) {
    // @tauri-apps/api 动态导入在打包时解析；运行时若无后端会 reject。
    return w.__TAURI__ ?? {};
  }
  return null;
}

/** 后端是否可达（由首次成功调用确认）。 */
export let backendAvailable = false;

export function markBackendAvailable(): void {
  backendAvailable = true;
}

export function markBackendUnavailable(): void {
  backendAvailable = false;
}

/**
 * 后端命令的真实错误信息。Tauri 对命令返回的 `IpcError { message }` 会 reject 成对象
 * `{ message }`，自己的参数解析错误则是字符串；两者都要取出文字，否则会显示 `[object Object]`。
 */
export function errorText(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  if (err && typeof err === "object" && typeof (err as { message?: unknown }).message === "string") {
    return (err as { message: string }).message;
  }
  return String(err);
}

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const mod = await import("@tauri-apps/api/core").catch(() => null);
  if (!mod || !tauri()) {
    markBackendUnavailable();
    throw new Error("backend_unavailable");
  }
  try {
    const result = await mod.invoke<T>(cmd, args);
    markBackendAvailable();
    return result;
  } catch (err) {
    // 命令失败 ≠ 后端不可达：保留真实错误并向上抛出。
    // 仅在后端确认不可达（后续探测失败）时才整体降级。
    throw err;
  }
}

export async function listen(cmd: string, handler: ListenFn): Promise<UnlistenFn> {
  const mod = await import("@tauri-apps/api/event").catch(() => null);
  if (!mod || !tauri()) {
    markBackendUnavailable();
    return () => {};
  }
  try {
    const un = await mod.listen(cmd, handler);
    markBackendAvailable();
    return () => {
      try {
        un();
      } catch {
        /* 后端已退出时忽略 */
      }
    };
  } catch (err) {
    markBackendUnavailable();
    throw err;
  }
}

/* ---------- 命令封装 ---------- */

export const commands = {
  listProfiles: () => invoke<Profile[]>("list_profiles"),
  saveProfile: (draft: ProfileDraft) =>
    invoke<Profile>("save_profile", { draft }),
  removeProfile: (id: string) => invoke<void>("remove_profile", { id }),
  /** 只检查本机现有 SSH 私钥是否加密，密钥内容不会离开后端。 */
  keyPassphraseRequired: (profileId: string) =>
    invoke<boolean>("key_passphrase_required", { profile_id: profileId }),
  /** 显式保存（记住）密码：写入系统凭据库，仅在用户勾选“记住密码”时调用。 */
  storeProfilePassword: (profileId: string, password: string) =>
    invoke<void>("store_profile_password", { profile_id: profileId, password }),

  /** 把界面语言同步给后端：后端错误消息与托盘菜单随之切换。 */
  setLanguage: (lang: string) => invoke<void>("set_language", { lang }),

  hostStatus: () => invoke<HostStatus>("host_status"),
  /** 初始化主机接收端密钥与接收密码（≥12 字符）。 */
  initHost: (password?: string, authorizedKeysPath?: string) =>
    invoke<HostStatus>("init_host", {
      password: password ?? null,
      authorized_keys_path: authorizedKeysPath ?? null,
    }),
  switchHostToKeys: (authorizedKeysPath: string) =>
    invoke<HostStatus>("switch_host_to_keys", { authorized_keys_path: authorizedKeysPath }),
  setHostEnabled: (enabled: boolean, bindAddr: string) =>
    invoke<HostStatus>("set_host_enabled", { enabled, bind_addr: bindAddr }),

  /** 仅探测：返回服务端实际指纹，不做认证、不落信任记录。 */
  probeHost: (host: string, port: number) =>
    invoke<FingerprintResult>("probe_host", { host, port }),
  /** 用户明确确认后调用：后端重新核对指纹一致才写入凭据库。 */
  confirmHostFingerprint: (host: string, port: number, fingerprint: string) =>
    invoke<FingerprintResult>("confirm_host_fingerprint", { host, port, fingerprint }),

  listSessions: () => invoke<SessionInfo[]>("list_sessions"),
  listRemoteSessions: (profileId: string, password?: string) =>
    invoke<RemoteSession[]>("list_remote_sessions", { profile_id: profileId, password: password ?? null }),
  connectExistingSession: (profileId: string, sessionId: string, password?: string) =>
    invoke<CreateSessionResult>("connect_existing_session", { profile_id: profileId, session_id: sessionId, password: password ?? null }),
  /** password 仅为密码认证档案的本次连接密码；不传则由后端回退到凭据库。
   *  rows/cols 是前端实际 fit 出的尺寸。 */
  createSession: (profileId: string, password?: string, rows?: number, cols?: number) =>
    invoke<CreateSessionResult>("create_session", {
      profile_id: profileId,
      password: password ?? null,
      rows: rows ?? 24,
      cols: cols ?? 80,
    }),
  /** resumeFrom：已显示到的输出偏移；服务端优先重放补洞，越界则给快照。 */
  attachSession: (sessionId: string, resumeFrom?: number | null) =>
    invoke<AttachResult>("attach_session", {
      session_id: sessionId,
      resume_from: resumeFrom ?? null,
    }),
  detachSession: (sessionId: string) =>
    invoke<SessionInfo>("detach_session", { session_id: sessionId }),
  endSession: (sessionId: string) =>
    invoke<SessionInfo>("end_session", { session_id: sessionId }),
  /** 原始终端输入字节；仅控制器可用，只读实例会被服务端拒绝。 */
  sendInput: (sessionId: string, data: number[]) =>
    invoke<void>("send_input", { session_id: sessionId, data }),
  /** 控制者按前端 fit 结果调整远端 PTY 尺寸。 */
  resizeSession: (sessionId: string, rows: number, cols: number) =>
    invoke<void>("resize_session", { session_id: sessionId, rows, cols }),
  takeControl: (sessionId: string) =>
    invoke<TakeControlResult>("take_control", { session_id: sessionId }),
};
