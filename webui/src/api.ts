// Typed client for the Service API. All paths are relative to the same
// origin the SPA was served from, so the built app works over the embedded
// service (Option A distribution) and over the Vite dev proxy.

import type {
  CacheList,
  ExecutionReport,
  ExecuteRequest,
  ExportResult,
  ScanSummary,
  Session,
  Suggestion,
  SuggestionList,
  ValidateResult,
} from "./types";

const BASE = "/api/v1";

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly details: Record<string, unknown> | undefined;

  constructor(
    status: number,
    code: string,
    message: string,
    details?: Record<string, unknown>,
  ) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
    this.details = details;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(BASE + path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init?.headers ?? {}),
    },
  });
  if (!res.ok) {
    let code = "error";
    let message = `HTTP ${res.status}`;
    let details: Record<string, unknown> | undefined;
    try {
      const body = (await res.json()) as { error?: ApiError };
      if (body && typeof body === "object" && "error" in body) {
        const err = (body as { error: ApiError }).error;
        code = err.code ?? code;
        message = err.message ?? message;
        details = err.details;
      }
    } catch {
      // Non-JSON error bodies fall back to the status line.
    }

    throw new ApiError(res.status, code, message, details);
  }
  if (res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

/** Filter body accepted by the selection endpoints; omitted fields are
 *  unconstrained, so an empty object matches every suggestion. */
export interface SelectionFilter {
  type?: string;
  asset_kind?: string;
  status?: string;
  selected?: boolean;
}

export const api = {
  async health(): Promise<void> {
    await request<void>("/health");
  },

  async session(): Promise<Session> {
    return request<Session>("/session");
  },

  async startScan(): Promise<void> {
    await request<void>("/session/scan", {
      method: "POST",
      body: JSON.stringify({}),
    });
  },

  async suggestions(params?: {
    type?: string;
    kind?: string;
    status?: string;
    selected?: boolean;
    offset?: number;
    limit?: number;
  }): Promise<SuggestionList> {
    const q = new URLSearchParams();
    if (params?.type) q.set("type", params.type);
    if (params?.kind) q.set("asset_kind", params.kind);
    if (params?.status) q.set("status", params.status);
    if (params?.selected !== undefined)
      q.set("selected", String(params.selected));
    if (params?.offset !== undefined) q.set("offset", String(params.offset));
    if (params?.limit !== undefined) q.set("limit", String(params.limit));
    const qs = q.toString();
    return request<SuggestionList>(`/suggestions${qs ? `?${qs}` : ""}`);
  },

  async selection(): Promise<{ ids: string[] }> {
    return request<{ ids: string[] }>("/selection");
  },

  /** Invalidate one suggestion's cached provider metadata and re-request
   *  it. When `clearOverrides` is true the service first removes every saved
   *  manual metadata override for the content, so the provider fields can be
   *  requested again. Returns the updated suggestion, now marked pending. */
  async refreshMetadata(
    id: string,
    clearOverrides = false,
  ): Promise<Suggestion> {
    const qs = clearOverrides ? "?clear_overrides=true" : "";
    const res = await request<{ item: Suggestion }>(
      `/suggestions/${encodeURIComponent(id)}/refresh${qs}`,
      { method: "POST" },
    );
    return res.item;
  },

  /** Replace one move suggestion's manual metadata overrides
   *  (location/subject/author; null or blank clears a field). Returns the
   *  rebuilt suggestion, or null when the edit makes the file stay put and
   *  the suggestion is removed. */
  async setSuggestionMetadata(
    id: string,
    edit: { location: string | null; subject: string | null; author: string | null },
  ): Promise<Suggestion | null> {
    const res = await request<{ item: Suggestion | null }>(
      `/suggestions/${encodeURIComponent(id)}/metadata`,
      { method: "PUT", body: JSON.stringify(edit) },
    );
    return res.item;
  },

  /** Re-apply the effective metadata (including persisted manual overrides)
   *  and rebuild one move suggestion without a scan or provider call. Returns
   *  the rebuilt suggestion, or null when it no longer applies. */
  async rebuildSuggestion(id: string): Promise<Suggestion | null> {
    const res = await request<{ item: Suggestion | null }>(
      `/suggestions/${encodeURIComponent(id)}/rebuild`,
      { method: "POST" },
    );
    return res.item;
  },

  async selectionPut(ids: string[]): Promise<void> {
    await request<void>("/selection", {
      method: "PUT",
      body: JSON.stringify({ ids }),
    });
  },

  async selectionAdd(ids: string[]): Promise<void> {
    await request<void>("/selection/add", {
      method: "POST",
      body: JSON.stringify({ ids }),
    });
  },

  /** Add every suggestion matching `filter` to the selection, without
   *  shipping the ids (see DESIGN.md "Service API > Suggestions &
   *  Selection"). */
  async selectionAddFilter(filter: SelectionFilter): Promise<void> {
    await request<void>("/selection/add", {
      method: "POST",
      body: JSON.stringify({ filter }),
    });
  },

  async selectionRemove(ids: string[]): Promise<void> {
    await request<void>("/selection/remove", {
      method: "POST",
      body: JSON.stringify({ ids }),
    });
  },

  async validate(ids?: string[]): Promise<ValidateResult> {
    return request<ValidateResult>("/validate", {
      method: "POST",
      body: JSON.stringify({ ids: ids ?? null }),
    });
  },

  async execute(ids: string[]): Promise<void> {
    const body: ExecuteRequest = { ids };
    await request<void>("/execute", {
      method: "POST",
      body: JSON.stringify(body),
    });
  },

  async cancel(): Promise<void> {
    await request<void>("/cancel", { method: "POST" });
  },

  async execution(): Promise<ExecutionReport> {
    return request<ExecutionReport>("/execution");
  },

  async summary(): Promise<ScanSummary> {
    return request<ScanSummary>("/report/summary");
  },

  async run(ids: string[]): Promise<void> {
    await this.execute(ids);
  },

  async exportScript(ids: string[]): Promise<ExportResult> {
    const res = await fetch(BASE + "/export", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ids }),
    });
    if (!res.ok) {
      let code = "error";
      let message = `HTTP ${res.status}`;
      try {
        const body = (await res.json()) as {
          error?: { code?: string; message?: string };
        };
        if (body?.error) {
          code = body.error.code ?? code;
          message = body.error.message ?? message;
        }
      } catch {
        // ignore
      }
      throw new ApiError(res.status, code, message);
    }
    const text = await res.text();
    const platformHeader = res.headers.get("x-script-platform");
    const countHeader = res.headers.get("x-script-count");
    return {
      platform: platformHeader ?? "unknown",
      count: countHeader ? Number(countHeader) : 0,
      text,
    };
  },

  async config(): Promise<Record<string, unknown>> {
    return request<Record<string, unknown>>("/config");
  },

  async configValidate(doc: Record<string, unknown>): Promise<ValidateResult> {
    return request<ValidateResult>("/config/validate", {
      method: "POST",
      body: JSON.stringify(doc),
    });
  },

  async configPut(
    doc: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    return request<Record<string, unknown>>("/config", {
      method: "PUT",
      body: JSON.stringify(doc),
    });
  },

  async cacheClear(): Promise<void> {
    await request<void>("/cache", { method: "DELETE" });
  },

  /** List cached provider results; `kind` and paging are optional. */
  async cacheList(params?: {
    kind?: string;
    offset?: number;
    limit?: number;
  }): Promise<CacheList> {
    const q = new URLSearchParams();
    if (params?.kind) q.set("kind", params.kind);
    if (params?.offset !== undefined) q.set("offset", String(params.offset));
    if (params?.limit !== undefined) q.set("limit", String(params.limit));
    const qs = q.toString();
    return request<CacheList>(`/cache${qs ? `?${qs}` : ""}`);
  },
};

export function resolveApiError(e: unknown): string {
  if (e instanceof ApiError) {
    const detail = e.details
      ? Object.entries(e.details)
          .map(([field, reason]) => `${field}: ${String(reason)}`)
          .join("; ")
      : "";
    return `${e.code}: ${e.message}${detail ? ` (${detail})` : ""}`;
  }
  if (e instanceof Error) {
    return e.message;
  }
  return String(e);
}

export type { Suggestion };
