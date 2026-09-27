import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import type { SessionOutputEvent } from "./types";

/**
 * 只读终端视窗。
 *
 * 安全约束：绝不注册会向主机转发键入的 onData 处理器，
 * xterm 也不获得任何写入主机的通道。主机输出仅经
 * backend → 事件 → term.write 单向进入视窗。
 */
export class TerminalView {
  readonly sessionId: string;
  private term: Terminal;
  private lastSeq = -1;
  private container: HTMLElement;

  constructor(sessionId: string, container: HTMLElement) {
    this.sessionId = sessionId;
    this.container = container;
    this.term = new Terminal({
      disableStdin: true,
      convertEol: false,
      scrollback: 5000,
      cursorBlink: false,
      fontSize: 13,
      fontFamily: 'Consolas, "Courier New", monospace',
    });
    // 防御性：即便日后有人误开 stdin，也吞掉数据、绝不外发。
    this.term.onData(() => {
      /* read-only：键盘输入被丢弃，不转发到主机 */
    });
    this.term.open(container);
    this.fit();
  }

  fit(): void {
    const fit = (this.term as unknown as { fit?: () => void }).fit;
    if (typeof fit === "function") fit.call(this.term);
  }

  /** 全量画面（attach 返回的 screen_b64）。 */
  setScreen(screenB64: string, seq: number): void {
    const bytes = base64ToBytes(screenB64);
    this.term.reset();
    this.term.write(bytes);
    this.lastSeq = seq;
  }

  /** 增量输出事件；乱序/重复序号被丢弃。 */
  applyOutput(ev: SessionOutputEvent): boolean {
    if (ev.session_id !== this.sessionId || ev.seq <= this.lastSeq) return true;
    if (this.lastSeq >= 0 && ev.seq !== this.lastSeq + 1) return false;
    this.lastSeq = ev.seq;
    this.term.write(base64ToBytes(ev.data_b64));
    return true;
  }

  getSeq(): number {
    return this.lastSeq;
  }

  dispose(): void {
    this.term.dispose();
    this.container.remove();
  }
}

/** base64 → UTF-8 字节（终端输出按 UTF-8 解码）。 */
function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}
