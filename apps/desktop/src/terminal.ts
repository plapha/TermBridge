import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import "@xterm/xterm/css/xterm.css";

/**
 * v2 原生交互终端视窗：
 * - 原始按键（onData/onBinary）只在控制者状态下发送；
 * - FitAddon + ResizeObserver（100 ms 防抖）驱动远端 Resize；
 * - Unicode11 宽度（中文/emoji）；
 * - 剪贴板与快捷键（Ctrl/Cmd+C/V、Ctrl+Shift+C/V）；
 * - 输出按偏移拼接，快照事件重置画面；本地积压超过 8 MiB 时上报重新挂接。
 */

const FONT_FAMILY = 'Consolas, "Courier New", monospace';
const FONT_SIZE = 13;
const MAX_QUEUED_BYTES = 8 * 1024 * 1024;

export interface TerminalCallbacks {
  /** 控制者的原始输入字节（UTF-8）。 */
  onInput: (data: Uint8Array) => void;
  /** 非控制者按键：提示观察模式。 */
  onBlockedInput: () => void;
  /** fit 变化（防抖后）：应发送 Resize（仅控制者）。 */
  onResize: () => void;
  /** 剪贴板不可用。 */
  onClipboardError: () => void;
}

export class TerminalView {
  readonly sessionId: string;
  private term: Terminal;
  private fitAddon: FitAddon;
  private container: HTMLElement;
  private callbacks: TerminalCallbacks;
  private controller = false;
  /** 已显示到的输出偏移；null 表示还没有可用的画面。 */
  private offset: number | null = null;
  private snapshot: {
    offset: number;
    rows: number;
    cols: number;
    chunks: Uint8Array[];
  } | null = null;
  private queuedBytes = 0;
  private resizeTimer: number | undefined;
  private resizeObserver: ResizeObserver | null = null;

  constructor(sessionId: string, container: HTMLElement, callbacks: TerminalCallbacks) {
    this.sessionId = sessionId;
    this.container = container;
    this.callbacks = callbacks;
    this.term = new Terminal({
      disableStdin: false,
      convertEol: false,
      scrollback: 5000,
      cursorBlink: true,
      fontSize: FONT_SIZE,
      fontFamily: FONT_FAMILY,
      macOptionIsMeta: true,
    });
    this.fitAddon = new FitAddon();
    this.term.loadAddon(this.fitAddon);
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";
    this.term.open(container);
    this.term.onData((data) => {
      if (!this.controller) {
        this.callbacks.onBlockedInput();
        return;
      }
      this.callbacks.onInput(new TextEncoder().encode(data));
    });
    this.term.onBinary((data) => {
      if (!this.controller) {
        this.callbacks.onBlockedInput();
        return;
      }
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
      this.callbacks.onInput(bytes);
    });
    this.term.attachCustomKeyEventHandler((event) => this.handleKey(event));
    // 非 Windows：至少拦掉会刷新/破坏页面的浏览器快捷键。
    container.addEventListener(
      "keydown",
      (event) => this.blockBrowserAccelerators(event as KeyboardEvent),
      true,
    );
    if (typeof ResizeObserver !== "undefined") {
      this.resizeObserver = new ResizeObserver(() => this.scheduleFit());
      this.resizeObserver.observe(container);
    }
    this.fit();
  }

  /* ---------- 尺寸 ---------- */

  fit(): void {
    try {
      this.fitAddon.fit();
    } catch {
      /* 容器尚未布局（display:none）时忽略 */
    }
  }

  getSize(): { rows: number; cols: number } {
    return { rows: this.term.rows, cols: this.term.cols };
  }

  private scheduleFit(): void {
    if (this.resizeTimer !== undefined) window.clearTimeout(this.resizeTimer);
    this.resizeTimer = window.setTimeout(() => {
      this.resizeTimer = undefined;
      const before = this.getSize();
      this.fit();
      const after = this.getSize();
      if (
        this.controller &&
        (before.rows !== after.rows || before.cols !== after.cols)
      ) {
        this.callbacks.onResize();
      }
    }, 100);
  }

  /* ---------- 控制者状态 ---------- */

  setController(value: boolean): void {
    this.controller = value;
  }

  isController(): boolean {
    return this.controller;
  }

  /* ---------- 键盘 / 剪贴板 ---------- */

  private handleKey(event: KeyboardEvent): boolean {
    if (event.type !== "keydown") return true;
    const isMac = navigator.platform.toUpperCase().includes("MAC");
    const mod = isMac ? event.metaKey : event.ctrlKey;
    if (!mod) return true;
    const key = event.key.toLowerCase();
    if (key === "c") {
      // 有选区（或显式 Ctrl+Shift+C）复制且不发送 ^C；否则按普通 ^C 发送。
      if (event.shiftKey || this.term.hasSelection()) {
        event.preventDefault();
        void this.copySelection();
        return false;
      }
      return true;
    }
    if (key === "v") {
      event.preventDefault();
      void this.pasteFromClipboard();
      return false;
    }
    return true;
  }

  private async copySelection(): Promise<void> {
    const selection = this.term.getSelection();
    if (!selection) return;
    try {
      await navigator.clipboard.writeText(selection);
    } catch {
      this.callbacks.onClipboardError();
    }
  }

  private async pasteFromClipboard(): Promise<void> {
    try {
      const text = await navigator.clipboard.readText();
      // term.paste 会按远端括号粘贴模式自动包裹。
      if (text) this.term.paste(text);
    } catch {
      this.callbacks.onClipboardError();
    }
  }

  private blockBrowserAccelerators(event: KeyboardEvent): void {
    const key = event.key.toLowerCase();
    const mod = event.ctrlKey || event.metaKey;
    if (
      event.key === "F5" ||
      (mod && ["r", "w", "p", "0", "+", "-", "="].includes(key))
    ) {
      event.preventDefault();
      event.stopPropagation();
    }
  }

  /** 供 main.ts 在状态栏提示后调用。 */
  focusTerminal(): void {
    this.term.focus();
  }

  /* ---------- 快照与输出 ---------- */

  beginSnapshot(offset: number, rows: number, cols: number): void {
    this.snapshot = { offset, rows, cols, chunks: [] };
    this.offset = null;
    this.term.reset();
    this.term.resize(cols, rows);
  }

  snapshotChunk(dataB64: string): void {
    this.snapshot?.chunks.push(base64ToBytes(dataB64));
  }

  endSnapshot(): number | null {
    const snapshot = this.snapshot;
    if (!snapshot) return this.offset;
    this.snapshot = null;
    const merged = concatChunks(snapshot.chunks);
    if (!this.write(merged)) return null;
    this.offset = snapshot.offset;
    return this.offset;
  }

  /**
   * 增量输出：返回 false 表示出现缺口或本地积压过多（调用方应重新挂接）。
   * 完全重复的字节被跳过。
   */
  applyOutput(offset: number, dataB64: string): boolean {
    if (this.offset === null || this.snapshot) return true;
    const bytes = base64ToBytes(dataB64);
    const end = offset + bytes.length;
    if (end <= this.offset) return true;
    if (offset > this.offset) return false;
    const skip = this.offset - offset;
    const slice = skip > 0 ? bytes.subarray(skip) : bytes;
    if (!this.write(slice)) return false;
    this.offset = offset + bytes.length;
    return true;
  }

  getOffset(): number | null {
    return this.offset;
  }

  /** 观察者/控制者按会话尺寸显示（3.5）。 */
  resizeTo(rows: number, cols: number): void {
    this.term.resize(cols, rows);
  }

  private write(bytes: Uint8Array): boolean {
    this.queuedBytes += bytes.length;
    this.term.write(bytes, () => {
      this.queuedBytes -= bytes.length;
    });
    return this.queuedBytes <= MAX_QUEUED_BYTES;
  }

  dispose(): void {
    if (this.resizeTimer !== undefined) window.clearTimeout(this.resizeTimer);
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.term.dispose();
    this.container.remove();
  }
}

/**
 * 用与终端相同的字体测量容器内实际可用的行列数（创建会话时使用）。
 */
export function measureTerminalSize(host: HTMLElement): { rows: number; cols: number } {
  const holder = document.createElement("div");
  holder.style.position = "absolute";
  holder.style.inset = "0";
  holder.style.visibility = "hidden";
  host.appendChild(holder);
  const term = new Terminal({
    fontSize: FONT_SIZE,
    fontFamily: FONT_FAMILY,
    scrollback: 0,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(holder);
  fit.fit();
  const dims = {
    rows: Math.max(2, term.rows),
    cols: Math.max(2, term.cols),
  };
  term.dispose();
  holder.remove();
  return dims;
}

/** base64 → 原始字节。 */
function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

function concatChunks(chunks: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const chunk of chunks) total += chunk.length;
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.length;
  }
  return out;
}
