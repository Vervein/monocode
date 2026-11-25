import { Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

/** The published owner descriptor file shape (`workspace-backends/<pid>.json`).
 * The token inside a user-only file is the IPC credential. */
export type BackendDescriptorFile = {
  version: number;
  pid: number;
  socketPath: string;
  token: string;
  workspaceSourceId: string;
  ownerEpoch: string;
  ownerMode?: string;
  appVersion: string;
  startedAt: number;
};

export const IPC_PROTOCOL_VERSION = 1;
const IPC_MAX_MESSAGE_BYTES = 16 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 60_000;

/** Mirrors `workspace_runtime_dir` in the Rust core. */
export function workspaceRuntimeDir(): string {
  if (process.env.MONOCODE_WORKSPACE_DIR) return process.env.MONOCODE_WORKSPACE_DIR;
  if (process.platform === "darwin")
    return join(homedir(), "Library", "Application Support", "Monocode", "runtime");
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && xdg.startsWith("/")) return join(xdg, "monocode");
  return join(homedir(), ".local", "share", "monocode", "runtime");
}

export function workspaceBackendsDir(): string {
  return join(workspaceRuntimeDir(), "workspace-backends");
}

export function parseDescriptor(raw: string): BackendDescriptorFile | null {
  try {
    const value = JSON.parse(raw) as BackendDescriptorFile;
    if (
      value?.version !== 1 ||
      typeof value.pid !== "number" ||
      typeof value.socketPath !== "string" ||
      typeof value.token !== "string" ||
      typeof value.workspaceSourceId !== "string" ||
      typeof value.ownerEpoch !== "string" ||
      !value.workspaceSourceId ||
      !value.socketPath
    )
      return null;
    return value;
  } catch {
    return null;
  }
}

export function pidAlive(pid: number): boolean {
  if (!Number.isSafeInteger(pid) || pid < 1) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

type Pending = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

/**
 * One connection to a workspace owner's IPC socket. Requests pipeline: each
 * carries an id and answers arrive in owner order, so a slow history read
 * never blocks a catalog refresh on the same connection.
 */
export class WorkspaceIpcClient {
  private socket: Socket | null = null;
  private connecting: Promise<Socket> | null = null;
  private buffer = Buffer.alloc(0);
  private nextId = 0;
  private readonly pending = new Map<number, Pending>();
  private broken = false;

  constructor(readonly descriptor: BackendDescriptorFile) {}

  get ownerEpoch(): string {
    return this.descriptor.ownerEpoch;
  }

  /** A dropped socket never serves again, but the owner behind it may be
   * fine — the caller uses this to retire the client object itself and open
   * a fresh connection to the same owner. */
  get closed(): boolean {
    return this.broken;
  }

  private open(): Promise<Socket> {
    if (this.socket) return Promise.resolve(this.socket);
    if (this.connecting) return this.connecting;
    if (this.broken) return Promise.reject(new Error("Workspace owner connection is closed"));
    const opening: Promise<Socket> = new Promise((resolve, reject) => {
      const socket = new Socket();
      const fail = (error: Error) => {
        socket.destroy();
        this.broken = true;
        reject(error);
      };
      socket.once("error", fail);
      socket.connect({ path: this.descriptor.socketPath }, () => {
        socket.off("error", fail);
        socket.write(
          JSON.stringify({
            token: this.descriptor.token,
            client: "monocode-host",
            protocol: IPC_PROTOCOL_VERSION,
          }) + "\n",
        );
        socket.on("data", (chunk: Buffer) => this.consume(chunk));
        socket.on("error", () => this.drop(new Error("Workspace owner connection failed")));
        socket.on("close", () => this.drop(new Error("Workspace owner disconnected")));
        this.socket = socket;
        resolve(socket);
      });
    });
    this.connecting = opening;
    opening.finally(() => {
      if (this.connecting === opening) this.connecting = null;
    }).catch(() => {});
    return opening;
  }

  private consume(chunk: Buffer) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    if (this.buffer.length > IPC_MAX_MESSAGE_BYTES) {
      this.drop(new Error("Workspace owner sent an oversized frame"));
      return;
    }
    for (;;) {
      const newline = this.buffer.indexOf(0x0a);
      if (newline < 0) return;
      const line = this.buffer.subarray(0, newline).toString("utf8");
      this.buffer = this.buffer.subarray(newline + 1);
      let frame: { id?: number; result?: unknown; error?: string };
      try {
        frame = JSON.parse(line);
      } catch {
        continue;
      }
      const id = Number(frame.id);
      const entry = this.pending.get(id);
      if (!entry) continue;
      this.pending.delete(id);
      clearTimeout(entry.timer);
      if (typeof frame.error === "string") entry.reject(new Error(frame.error));
      else entry.resolve(frame.result ?? null);
    }
  }

  private drop(error: Error) {
    this.broken = true;
    for (const [id, entry] of this.pending) {
      this.pending.delete(id);
      clearTimeout(entry.timer);
      entry.reject(error);
    }
    this.socket?.destroy();
    this.socket = null;
  }

  async request(method: string, params: unknown): Promise<unknown> {
    const socket = await this.open();
    const id = ++this.nextId;
    return await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Workspace owner did not answer ${method}`));
      }, REQUEST_TIMEOUT_MS);
      this.pending.set(id, { resolve, reject, timer });
      socket.write(JSON.stringify({ id, method, params: params ?? {} }) + "\n", (error) => {
        if (!error) return;
        this.pending.delete(id);
        clearTimeout(timer);
        reject(new Error(`Workspace owner write failed: ${error.message}`));
      });
    });
  }

  close() {
    this.drop(new Error("Workspace owner connection is closed"));
  }
}
