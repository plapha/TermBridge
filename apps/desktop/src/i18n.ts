/**
 * 界面语言：英文（默认）与简体中文。
 *
 * - 文案集中在下面的两张表里；`zh` 的类型是 `Record<MsgKey, string>`，
 *   缺键或多键都会在 `tsc` 时报错，保证两种语言同步。
 * - 静态文案写在 index.html 的 `data-i18n*` 属性上，由 `applyStatic` 套用；
 *   动态文案在代码里调用 `t(key, 参数)`。
 * - 语言选择保存在 localStorage；没有保存时跟随浏览器（系统）语言，其余回退英文。
 *   切换语言时通过 `onLangChange` 通知各处重绘，并同步给后端（后端消息与托盘菜单）。
 * - 新增语言：在 `Lang`、`LANGS`、`catalogs` 里各加一项并补一张表。
 */

export type Lang = "en" | "zh";

/** 语言选择器里显示的名称用各语言自己的写法，不随界面语言翻译。 */
export const LANGS: ReadonlyArray<{ code: Lang; label: string }> = [
  { code: "en", label: "English" },
  { code: "zh", label: "简体中文" },
];

const en = {
  "app.title": "TermBridge Desktop Terminal",
  "lang.label": "Language",

  "topbar.hideToTray": "Hide to tray",
  "topbar.hideToTrayTitle": "Hide the window to the tray; restore it from the tray icon",
  "backend.connected": "Local core connected",
  "backend.disconnected": "Backend not connected",
  "backend.unreachable": "TermBridge's local backend (crates/app) is not running or unreachable.",
  "backend.readonlyHint":
    "The UI is in a read-only placeholder state and all actions are disabled; no fake connection results are ever shown.",
  "banner.offline":
    "Connection lost and the screen is paused. Re-attach from “Existing terminals”; commands are never resent automatically.",

  "tabs.aria": "Session tabs",
  "tabs.empty": "No sessions yet. Choose a connection profile on the left and click “New terminal”.",
  "tab.controller": "Controller",
  "tab.readonly": "Read-only",
  "tab.takeControl": "Take control",
  "tab.detach": "Detach",
  "tab.detachTitle": "Detach: keep the session and just stop viewing",
  "tab.terminate": "Terminate",
  "tab.terminateTitle": "Terminate: end this session",
  "state.connecting": "Connecting",
  "state.attached": "Attached",
  "state.detached": "Detached",
  "state.closed": "Ended",
  "state.error": "Error",

  "common.save": "Save",
  "common.cancel": "Cancel",

  "profiles.title": "Connection profiles",
  "profiles.new": "New",
  "profiles.listAria": "Saved connection profiles",
  "profiles.formAria": "Edit connection profile",
  "profiles.formNew": "New connection profile",
  "profiles.formEdit": "Edit connection profile",
  "profiles.empty": "No saved connection profiles.",
  "profile.newTerminal": "New terminal",
  "profile.existing": "Existing terminals",
  "profile.edit": "Edit",
  "profile.remove": "Delete",
  "field.name": "Name",
  "field.host": "Host / IP",
  "field.port": "Port",
  "field.user": "Username",
  "field.auth": "Authentication",
  "field.authPassword":
    "Separate password (for legacy profiles; stored only if “Remember password” is ticked)",
  "field.authKey": "Reuse an SSH private key (recommended; only the path is referenced, nothing is copied)",
  "field.keyPath": "SSH private key path (optional; ~/.ssh/id_ed25519 and similar are found automatically)",
  "field.keyPathPlaceholder": "Leave empty to use an existing SSH key",
  "profiles.passwordHint":
    "Passwords go into the system credential store only when “Remember password” is explicitly chosen, never into the config file.",

  "terminal.aria": "Terminal view",
  "terminal.placeholder": "Not attached to any session yet.",
  "terminal.placeholderHint":
    "The terminal supports native interaction (Ctrl+C, Tab, arrow keys, …); only the controller's input is sent to the host.",
  "hint.queueFull": "The local input queue is full; this keystroke was not sent.",
  "hint.observer": "Observer mode: click “Take control” to type.",
  "hint.clipboard": "Cannot access the system clipboard.",
  "hint.offline": "Connection lost; commands are never resent automatically.",

  "host.aria": "Host mode",
  "host.title": "Local receiver",
  "host.running": "Running",
  "host.stopped": "Stopped",
  "host.unknown": "Unknown",
  "host.initTitle": "Initialize the host",
  "host.authLegend": "Choose host authentication",
  "host.authKeys": "Reuse existing SSH authorized keys (recommended; no extra password)",
  "host.authPassword": "Separate password",
  "host.authorizedKeysPath": "SSH authorized_keys file path",
  "host.authorizedKeysPlaceholder": "Leave empty to use ~/.ssh/authorized_keys",
  "host.password": "Host password (at least 12 characters)",
  "host.initHint":
    "Authorized keys are read only when you explicitly choose this; entries with SSH restrictions such as from= or command= are rejected so existing permissions are never widened. Private keys are never copied into this product.",
  "host.init": "Initialize",
  "host.listen": "Listen address",
  "host.listenAria": "Choose listen address",
  "host.bindLocal": "Local only (127.0.0.1)",
  "host.bindAll": "All interfaces (0.0.0.0)",
  "host.bindLocal6": "Local only, IPv6 (::1)",
  "host.bindCustom": "Tailscale / custom IP…",
  "host.customIp": "Custom listen IP",
  "host.customIpPlaceholder": "100.x.y.z (Tailscale IP)",
  "host.port": "Listen port",
  "host.metaStatus": "Status",
  "host.metaFingerprint": "Fingerprint",
  "host.metaAuth": "Authentication",
  "host.metaController": "Controller",
  "host.enable": "Enable",
  "host.stop": "Stop local receiver",
  "host.migrateTitle": "Existing host: switch to existing SSH keys",
  "host.migrateHint":
    "Stop the local receiver first; importing keys turns off the old separate password. If a CLI service runs separately, restart it by hand for this to take effect.",
  "host.migrate": "Switch to SSH keys only",
  "host.placeholder": "The backend is unavailable; host mode cannot be managed.",
  "host.authStatusPassword": "Separate password",
  "host.authStatusPasswordKeys": "Separate password + {count} authorized key(s)",
  "host.authStatusKeys": "SSH keys only ({count})",
  "host.passwordShort": "The host password must be at least 12 characters",
  "host.needAddress":
    "Enabling the host needs a full listen address (IP:port, for example 127.0.0.1:22333)",
  "host.confirmMigrate":
    "Disable the separate password and accept only the SSH authorized keys in the selected file? Make sure you hold the matching private keys first.",

  "fp.title": "Confirm host fingerprint",
  "fp.hint":
    "Check that this fingerprint matches a trusted source before confirming; this app never trusts any host automatically.",
  "fp.confirm": "Confirm and trust this host",

  "existing.title": "Attach to an existing terminal",
  "existing.hint": "Choose a running remote session; no replacement terminal is created.",
  "existing.label": "Remote session",
  "existing.attach": "Attach",
  "existing.none": "No running sessions; old terminals are not recreated after a device restart",

  "pw.titlePassword": "Connection password",
  "pw.titleKey": "SSH key passphrase",
  "pw.labelPassword": "Password",
  "pw.labelKey": "Key passphrase",
  "pw.remember": "Remember password (store in the system credential store)",
  "pw.hintPassword":
    "Unless you tick the box, the password is used for this connection only and is never saved or logged.",
  "pw.hintKey":
    "The passphrase is only used to decrypt the SSH private key this time and is never saved or logged.",
  "pw.connect": "Connect",
  "pw.needKey": "Enter the SSH key passphrase",
  "pw.needPassword": "Missing connection password: enter it before connecting",
  "pw.confirmUseSaved":
    "Use the password remembered in the system credential store? Cancel to enter it again.",
  "pw.rememberFailed":
    "Connecting continues, but the system credential store could not save the password: {detail}",

  "confirm.end":
    "End the remote terminal process? Detaching keeps the session; ending it cannot be undone.",

  "error.withPrefix": "{prefix}: {detail}",
  "error.inputRejected": "Input rejected ({code}): {message}",
  "error.inputSend": "Failed to send input (state unknown; not resent automatically)",
  "error.resize": "Failed to resize the terminal",
  "error.resync": "Failed to resynchronize the screen",
  "error.loadProfiles": "Failed to load connection profiles",
  "error.saveProfile": "Failed to save the connection profile",
  "error.removeProfile": "Failed to delete the connection profile",
  "error.connect": "Connection failed",
  "error.attach": "Failed to attach to the session",
  "error.takeControl": "Failed to take control",
  "error.detach": "Failed to detach the session",
  "error.end": "Failed to end the session",
  "error.hostStatus": "Failed to read the host status",
  "error.initHost": "Failed to initialize the host",
  "error.switchKeys": "Failed to switch to key authentication",
  "error.enableHost": "Failed to enable the host",
  "error.stopHost": "Failed to stop the host",
  "error.hideToTray": "Failed to hide to the tray",
};

export type MsgKey = keyof typeof en;

const zh: Record<MsgKey, string> = {
  "app.title": "TermBridge 桌面终端",
  "lang.label": "语言",

  "topbar.hideToTray": "隐藏到托盘",
  "topbar.hideToTrayTitle": "隐藏窗口到托盘，托盘图标可找回",
  "backend.connected": "本地核心已连接",
  "backend.disconnected": "后端未连接",
  "backend.unreachable": "TermBridge 的本地后端（crates/app）尚未运行或不可达。",
  "backend.readonlyHint": "界面处于只读占位状态，所有操作已禁用；不会显示任何伪造的连接结果。",
  "banner.offline": "连接已断开，画面已暂停。请从“已有终端”重新附着，不会自动重发命令。",

  "tabs.aria": "会话标签",
  "tabs.empty": "暂无会话，请从左侧选择一个连接配置并点击“新建终端”。",
  "tab.controller": "控制器",
  "tab.readonly": "只读",
  "tab.takeControl": "接管输入",
  "tab.detach": "分离",
  "tab.detachTitle": "分离：保留会话，仅停止查看",
  "tab.terminate": "终止",
  "tab.terminateTitle": "终止：结束该会话",
  "state.connecting": "连接中",
  "state.attached": "已附加",
  "state.detached": "已分离",
  "state.closed": "已结束",
  "state.error": "错误",

  "common.save": "保存",
  "common.cancel": "取消",

  "profiles.title": "连接配置",
  "profiles.new": "新建",
  "profiles.listAria": "已保存的连接配置列表",
  "profiles.formAria": "连接配置编辑",
  "profiles.formNew": "新建连接配置",
  "profiles.formEdit": "编辑连接配置",
  "profiles.empty": "暂无保存的连接配置。",
  "profile.newTerminal": "新建终端",
  "profile.existing": "已有终端",
  "profile.edit": "编辑",
  "profile.remove": "删除",
  "field.name": "名称",
  "field.host": "主机 / IP",
  "field.port": "端口",
  "field.user": "用户名",
  "field.auth": "认证方式",
  "field.authPassword": "产品专用密码（兼容旧配置；仅勾选“记住密码”才保存）",
  "field.authKey": "复用 SSH 私钥（推荐；只引用路径，不复制内容）",
  "field.keyPath": "SSH 私钥路径（可留空，自动寻找 ~/.ssh/id_ed25519 等）",
  "field.keyPathPlaceholder": "留空使用已有 SSH 私钥",
  "profiles.passwordHint": "仅在明确选择“记住密码”时才存入系统凭据库，不写入配置文件。",

  "terminal.aria": "终端视窗",
  "terminal.placeholder": "尚未附加到任何会话。",
  "terminal.placeholderHint":
    "终端支持原生交互（Ctrl+C、Tab、方向键等）；只有控制者的输入会发送到主机。",
  "hint.queueFull": "本地输入队列已满，当前按键未发送。",
  "hint.observer": "观察模式：点「接管输入」后可输入。",
  "hint.clipboard": "无法访问系统剪贴板。",
  "hint.offline": "连接已断开；不会自动重发命令。",

  "host.aria": "主机模式",
  "host.title": "本应用接收端",
  "host.running": "运行中",
  "host.stopped": "已停止",
  "host.unknown": "未知",
  "host.initTitle": "初始化接收端",
  "host.authLegend": "选择接收端认证",
  "host.authKeys": "复用现有 SSH 授权公钥（推荐，无需额外密码）",
  "host.authPassword": "产品专用密码",
  "host.authorizedKeysPath": "SSH authorized_keys 文件路径",
  "host.authorizedKeysPlaceholder": "留空使用 ~/.ssh/authorized_keys",
  "host.password": "接收密码（至少 12 字符）",
  "host.initHint":
    "只在你明确选择时读取授权公钥；带 from=/command= 等 SSH 限制选项的条目会被拒绝，避免放宽原有权限。私钥不会复制到本产品。",
  "host.init": "初始化",
  "host.listen": "监听地址",
  "host.listenAria": "监听地址选择",
  "host.bindLocal": "仅本机 (127.0.0.1)",
  "host.bindAll": "所有网卡 (0.0.0.0)",
  "host.bindLocal6": "仅本机 IPv6 (::1)",
  "host.bindCustom": "Tailscale / 自定义 IP…",
  "host.customIp": "自定义监听 IP",
  "host.customIpPlaceholder": "100.x.y.z（Tailscale IP）",
  "host.port": "监听端口",
  "host.metaStatus": "状态",
  "host.metaFingerprint": "指纹",
  "host.metaAuth": "认证",
  "host.metaController": "控制器",
  "host.enable": "启用",
  "host.stop": "停止本应用接收",
  "host.migrateTitle": "已有接收端：改用现有 SSH 密钥",
  "host.migrateHint":
    "先停止本应用接收；导入公钥后会关闭旧产品密码。若 CLI 服务另行运行，也需手动重启才能生效。",
  "host.migrate": "改为仅 SSH 密钥",
  "host.placeholder": "后端不可用，无法管理主机模式。",
  "host.authStatusPassword": "产品密码",
  "host.authStatusPasswordKeys": "产品密码 + {count} 把公钥",
  "host.authStatusKeys": "仅 SSH 密钥（{count} 把）",
  "host.passwordShort": "接收密码至少需要 12 个字符",
  "host.needAddress": "启用接收端必须提供完整监听地址（IP:端口，如 127.0.0.1:22333）",
  "host.confirmMigrate":
    "确定停用产品专用密码，改为只接受所选文件中的 SSH 授权公钥？请先确保你持有对应私钥。",

  "fp.title": "确认主机指纹",
  "fp.hint": "请核对该指纹与受信来源一致后再确认；本应用不会自动信任任何主机。",
  "fp.confirm": "确认并信任该主机",

  "existing.title": "附着已有终端",
  "existing.hint": "选择仍在运行的远端会话；不会新建替代终端。",
  "existing.label": "远端会话",
  "existing.attach": "附着",
  "existing.none": "没有仍在运行的会话；设备重启后旧终端不会自动重建",

  "pw.titlePassword": "连接密码",
  "pw.titleKey": "SSH 私钥口令",
  "pw.labelPassword": "密码",
  "pw.labelKey": "私钥口令",
  "pw.remember": "记住密码（写入系统凭据库）",
  "pw.hintPassword": "不勾选时密码仅用于本次连接，不会被保存或记录。",
  "pw.hintKey": "口令只用于本次解密 SSH 私钥，不会保存或记录。",
  "pw.connect": "连接",
  "pw.needKey": "请输入 SSH 私钥口令",
  "pw.needPassword": "缺少连接密码：请输入密码后再连接",
  "pw.confirmUseSaved": "使用系统凭据库中已记住的密码？取消则重新输入。",
  "pw.rememberFailed": "本次连接继续，但系统凭据库无法保存密码：{detail}",

  "confirm.end": "确定结束远端终端进程？分离可保留会话，结束后无法恢复。",

  "error.withPrefix": "{prefix}：{detail}",
  "error.inputRejected": "输入被拒绝（{code}）：{message}",
  "error.inputSend": "输入发送失败（状态未知，不自动重发）",
  "error.resize": "调整终端尺寸失败",
  "error.resync": "画面重新同步失败",
  "error.loadProfiles": "读取连接配置失败",
  "error.saveProfile": "保存连接配置失败",
  "error.removeProfile": "删除连接配置失败",
  "error.connect": "连接失败",
  "error.attach": "附加会话失败",
  "error.takeControl": "接管输入失败",
  "error.detach": "分离会话失败",
  "error.end": "结束会话失败",
  "error.hostStatus": "读取接收状态失败",
  "error.initHost": "初始化接收端失败",
  "error.switchKeys": "切换密钥认证失败",
  "error.enableHost": "启用接收端失败",
  "error.stopHost": "停止接收端失败",
  "error.hideToTray": "隐藏到托盘失败",
};

const catalogs: Record<Lang, Record<MsgKey, string>> = { en, zh };

const STORAGE_KEY = "termbridge.lang";

/** `zh`、`zh-CN`、`zh-Hans` 等都归为 zh，其余归为 en。 */
export function parseLang(value: string | null | undefined): Lang | null {
  const primary = (value ?? "").toLowerCase().split(/[-_.]/)[0];
  if (primary === "zh") return "zh";
  if (primary === "en") return "en";
  return null;
}

function storedLang(): Lang | null {
  try {
    return parseLang(window.localStorage.getItem(STORAGE_KEY));
  } catch {
    return null; // 隐私模式等场景下 localStorage 可能不可用
  }
}

function systemLang(): Lang {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const lang = parseLang(tag);
    if (lang) return lang;
  }
  return "en";
}

let current: Lang = storedLang() ?? systemLang();
const listeners: Array<(lang: Lang) => void> = [];

export function getLang(): Lang {
  return current;
}

/** 取当前语言的文案；`{name}` 占位符用 params 替换。 */
export function t(key: MsgKey, params?: Record<string, string | number>): string {
  const text = catalogs[current][key] ?? catalogs.en[key] ?? key;
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) =>
    name in params ? String(params[name]) : whole,
  );
}

/** 语言变化后的重绘回调（动态生成的文案需要在这里重新渲染）。 */
export function onLangChange(callback: (lang: Lang) => void): void {
  listeners.push(callback);
}

export function setLang(lang: Lang): void {
  current = lang;
  try {
    window.localStorage.setItem(STORAGE_KEY, lang);
  } catch {
    /* 保存失败只影响下次启动的默认值 */
  }
  applyStatic();
  for (const callback of listeners) callback(lang);
}

/**
 * 套用 index.html 里的静态文案：
 * `data-i18n` 设文本，`data-i18n-title/-placeholder/-aria-label` 设对应属性。
 * 带输入框的 label 要把文字放进 `<span data-i18n>`，避免覆盖子元素。
 */
export function applyStatic(root: ParentNode = document): void {
  document.documentElement.lang = current === "zh" ? "zh-CN" : "en";
  document.title = t("app.title");
  const attrs: Array<[string, string]> = [
    ["data-i18n-title", "title"],
    ["data-i18n-placeholder", "placeholder"],
    ["data-i18n-aria-label", "aria-label"],
  ];
  root.querySelectorAll<HTMLElement>("[data-i18n]").forEach((el) => {
    el.textContent = t(el.dataset.i18n as MsgKey);
  });
  for (const [dataAttr, attr] of attrs) {
    root.querySelectorAll<HTMLElement>(`[${dataAttr}]`).forEach((el) => {
      el.setAttribute(attr, t(el.getAttribute(dataAttr) as MsgKey));
    });
  }
}
