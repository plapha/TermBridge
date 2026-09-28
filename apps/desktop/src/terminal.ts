import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

/**
 * v2 终端视窗：输出按偏移拼接，快照事件重置画面。
 *
 * M1 仍不接收键盘输入（M2 打开 disableStdin 并接 onData/FitAddon）；
 * 本视窗只负责：快照 Begin/Chunk/End、按偏移写入 Output、重复跳过、缺口上报。
 */
export class TerminalView {
  readonly sessionId: string;
  private term: Terminal;
  private container: HTMLElement;
  /** 已显示到的输出偏移；null 表示还没有可用的画面。 */
  private offset: number | null = null;
  private snapshot: {
    offset: number;
    rows: number;
    cols: number;
    chunks: Uint8Array[];
  } | null = null;

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

  /** SnapshotBegin：reset 并 resize 到服务端尺寸，等待分块。 */
  beginSnapshot(offset: number, rows: number, cols: number): void {
    this.snapshot = { offset, rows, cols, chunks: [] };
    this.offset = null;
    this.term.reset();
    this.term.resize(cols, rows);
  }

  snapshotChunk(dataB64: string): void {
    this.snapshot?.chunks.push(base64ToBytes(dataB64));
  }

  /** SnapshotEnd：写入所有分块，之后 Output 从快照 offset 继续。 */
  endSnapshot(): number | null {
    const snapshot = this.snapshot;
    if (!snapshot) return this.offset;
    this.snapshot = null;
    for (const chunk of snapshot.chunks) this.term.write(chunk);
    this.offset = snapshot.offset;
    return this.offset;
  }

  /**
   * 增量输出：返回 false 表示出现缺口（调用方应重新挂接补洞）。
   * 完全重复的字节被跳过。
   */
  applyOutput(offset: number, dataB64: string): boolean {
    if (this.offset === null || this.snapshot) return true;
    const bytes = base64ToBytes(dataB64);
    const end = offset + bytes.length;
    if (end <= this.offset) return true;
    if (offset > this.offset) return false;
    const skip = this.offset - offset;
    this.term.write(bytes.subarray(skip));
    this.offset = end;
    return true;
  }

  getOffset(): number | null {
    return this.offset;
  }

  /** 观察者/控制者按会话尺寸显示（3.5）。 */
  resizeTo(rows: number, cols: number): void {
    this.term.resize(cols, rows);
  }

  dispose(): void {
    this.term.dispose();
    this.container.remove();
  }
}

/** base64 → 原始字节。 */
function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}
