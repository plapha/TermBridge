/** 前端与后端（crates/app 暴露的 Tauri 命令）之间的类型契约。 */

/**
 * 仅 password / key。后端 save_profile 只接受这两种（`agent` 会被拒绝），
 * 因此前端不提供 SSH Agent 选项（尚未实现，不伪造可用）。
 */
export type AuthChoice = "password" | "key";

export interface Profile {
  id: string;
  name: string;
  host: string;
  port: number;
  user: string;
  auth: AuthChoice;
  key_path?: string | null;
  remember_password?: boolean;
}

/** save_profile 的入参：不含任何凭据内容。 */
export interface ProfileDraft {
  id?: string;
  name: string;
  host: string;
  port: number;
  user: string;
  auth: AuthChoice;
  key_path?: string | null;
  remember_password?: boolean;
}

export type SessionState =
  | "connecting"
  | "attached"
  | "detached"
  | "closed"
  | "error";

export interface SessionInfo {
  session_id: string;
  profile_id: string;
  profile_name: string;
  state: SessionState;
  /** 当前控制器；为空表示只读或无控制器。 */
  controller: string | null;
  /** 本前端实例是否为该会话的控制器。 */
  is_controller: boolean;
  /** 后端是否可达（离线时为 false；须显式从已有终端重新连接）。 */
  online: boolean;
}

export interface HostStatus {
  enabled: boolean;
  bind_addr: string;
  fingerprint: string | null;
  controller: string | null;
  password_enabled: boolean;
  authorized_key_count: number;
  /** 端口等附加信息，可选。 */
  port?: number;
}

/** session_output 事件负载。 */
export interface SessionOutputEvent {
  session_id: string;
  seq: number;
  data_b64: string;
}

/** 会话结束 / 断开事件负载（后端可选发布）。 */
export interface SessionClosedEvent {
  session_id: string;
  reason: string | null;
}

/** attach_session 命令的返回：全量画面快照 + 最新序号。 */
export interface AttachResult {
  screen_b64: string;
  seq: number;
  session: SessionInfo;
}

/** create_session 命令的返回。 */
export interface CreateSessionResult {
  session: SessionInfo;
}

/** take_control 命令的返回。 */
export interface TakeControlResult {
  session: SessionInfo;
}

/** probe_host / confirm_host_fingerprint 的返回。 */
export interface FingerprintResult {
  fingerprint: string;
  trusted: boolean;
}

export type ListenFn = (event: unknown) => void;

export type UnlistenFn = () => void;

export interface RemoteSession { id: string; title: string; live: boolean; controller: string | null; }
