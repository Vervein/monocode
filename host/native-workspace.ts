import { readdirSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import type { HostStore } from "./store";
import {
  WorkspaceIpcClient,
  parseDescriptor,
  pidAlive,
  workspaceBackendsDir,
  type BackendDescriptorFile,
} from "./workspace-backend";
import type {
  WorkspaceCandidate,
  WorkspaceDescriptor,
  WorkspaceOwnerMode,
  WorkspaceReadiness,
} from "../src/features/connections/model/protocol";

const BINDING_KEY = "workspaceBinding";

/**
 * The host's relationship with the real workspace owner: discovery of
 * published backends, the one-time binding to a stable source, and the IPC
 * proxy every workspace RPC flows through. The host never reimplements Mono
 * logic; when the owner is gone it reports exactly that.
 */
export class NativeWorkspace {
  private client: WorkspaceIpcClient | null = null;
  private clientPid = 0;
  private readonly backendsDir: string;

  constructor(
    private readonly store: HostStore,
    options: { backendsDir?: string } = {},
  ) {
    this.backendsDir = options.backendsDir ?? workspaceBackendsDir();
  }

  private metadata(key: string): string | null {
    const row = this.store.db
      .prepare("SELECT value FROM metadata WHERE key=?")
      .get(key) as { value: string } | undefined;
    return row?.value ?? null;
  }

  private setMetadata(key: string, value: string) {
    this.store.db
      .prepare(
        "INSERT INTO metadata VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
      )
      .run(key, value);
  }

  binding(): { workspaceSourceId: string; boundAt: number } | null {
    const raw = this.metadata(BINDING_KEY);
    if (!raw) return null;
    try {
      const value = JSON.parse(raw);
      return typeof value?.workspaceSourceId === "string" ? value : null;
    } catch {
      return null;
    }
  }

  /** Explicit one-time binding to a verified candidate. The same source
   * rebinds freely; a different source is a deliberate maintenance act. */
  bind(workspaceSourceId: string): void {
    const candidate = this.candidates().find(
      (entry) => entry.workspaceSourceId === workspaceSourceId,
    );
    if (!candidate)
      throw new Error("This workspace owner is not currently available");
    this.setMetadata(
      BINDING_KEY,
      JSON.stringify({ workspaceSourceId, boundAt: Date.now() }),
    );
  }

  private describeBase(): WorkspaceDescriptor {
    return {
      workspaceSourceId: null,
      ownerMode: "unbound",
      ownerEpoch: null,
      dataSchemaVersion: null,
      readiness: "unbound",
      capabilities: [],
    };
  }

  /** Live descriptors whose process exists. A connectable check happens on
   * use, not here, so discovery stays cheap and synchronous. */
  candidates(): BackendDescriptorFile[] {
    let names: string[] = [];
    try {
      names = readdirSync(this.backendsDir);
    } catch {
      return [];
    }
    const found: BackendDescriptorFile[] = [];
    for (const name of names) {
      if (!name.endsWith(".json")) continue;
      let raw: string;
      try {
        raw = readFileSync(join(this.backendsDir, name), "utf8");
      } catch {
        continue;
      }
      const descriptor = parseDescriptor(raw);
      if (!descriptor) continue;
      if (!pidAlive(descriptor.pid)) {
        // A dead owner cannot serve again; its descriptor only confuses
        // discovery, so remove it rather than surfacing a ghost candidate.
        rmSync(join(this.backendsDir, name), { force: true });
        continue;
      }
      found.push(descriptor);
    }
    return found.sort((a, b) => a.startedAt - b.startedAt);
  }

  private connection(descriptor: BackendDescriptorFile): WorkspaceIpcClient {
    // Any restart or replacement of the owner invalidates the old connection:
    // the epoch inside the new descriptor differs, and the old socket is dead.
    if (this.client && this.clientPid === descriptor.pid && this.client.ownerEpoch === descriptor.ownerEpoch)
      return this.client;
    this.client?.close();
    this.client = new WorkspaceIpcClient(descriptor);
    this.clientPid = descriptor.pid;
    return this.client;
  }

  /** Resolves the bound owner's descriptor, auto-binding a lone candidate.
   * Returns why no owner is usable when there is none. */
  private owner(): { descriptor: BackendDescriptorFile } | { error: WorkspaceReadiness; detail: string; candidates: BackendDescriptorFile[] } {
    const candidates = this.candidates();
    const binding = this.binding();
    if (binding) {
      const descriptor = candidates.find(
        (entry) => entry.workspaceSourceId === binding.workspaceSourceId,
      );
      if (descriptor) return { descriptor };
      return {
        error: "owner_unavailable",
        detail: "The bound workspace owner is not running on this machine",
        candidates,
      };
    }
    if (candidates.length === 1) {
      this.setMetadata(
        BINDING_KEY,
        JSON.stringify({
          workspaceSourceId: candidates[0].workspaceSourceId,
          boundAt: Date.now(),
        }),
      );
      return { descriptor: candidates[0] };
    }
    return {
      error: "unbound",
      detail:
        candidates.length === 0
          ? "No Monocode workspace is enabled on this machine yet"
          : "Several workspace owners are available; bind one explicitly",
      candidates,
    };
  }

  private candidateView(entry: BackendDescriptorFile): WorkspaceCandidate {
    return {
      workspaceSourceId: entry.workspaceSourceId,
      ownerMode: (entry.ownerMode === "service" ? "service" : "desktop") as WorkspaceOwnerMode,
      appVersion: entry.appVersion,
      startedAt: entry.startedAt,
    };
  }

  /** Never throws for an absent owner: absence is a state, not an error. */
  async describe(): Promise<WorkspaceDescriptor> {
    const owner = this.owner();
    if ("error" in owner) {
      return {
        ...this.describeBase(),
        readiness: owner.error,
        detail: owner.detail,
        candidates: owner.candidates.map((entry) => this.candidateView(entry)),
      };
    }
    try {
      const answer = (await this.connection(owner.descriptor).request(
        "workspace.describe",
        {},
      )) as Partial<WorkspaceDescriptor> | null;
      return {
        workspaceSourceId: owner.descriptor.workspaceSourceId,
        ownerMode: "desktop",
        ownerEpoch: owner.descriptor.ownerEpoch,
        dataSchemaVersion:
          typeof answer?.dataSchemaVersion === "number" ? answer.dataSchemaVersion : null,
        readiness: "ready",
        capabilities: Array.isArray(answer?.capabilities)
          ? answer.capabilities.filter((cap): cap is string => typeof cap === "string")
          : [],
        detail: undefined,
      };
    } catch (error) {
      return {
        ...this.describeBase(),
        workspaceSourceId: owner.descriptor.workspaceSourceId,
        ownerMode: "desktop",
        ownerEpoch: owner.descriptor.ownerEpoch,
        readiness: "owner_unavailable",
        detail: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /** Every workspace business RPC: proxied to the owner or refused with the
   * readiness state. Nothing falls back to host-local data. */
  async call(method: string, params: unknown): Promise<unknown> {
    const owner = this.owner();
    if ("error" in owner) {
      if (owner.error === "unbound" && owner.candidates.length === 0)
        throw new Error("No Monocode workspace is enabled on this machine yet");
      if (owner.error === "unbound")
        throw new Error("Several workspace owners are available; bind one explicitly");
      throw new Error("The workspace owner is not running on this machine");
    }
    return await this.connection(owner.descriptor).request(method, params);
  }
}
