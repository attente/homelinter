// Typed client for the Service API. All paths are relative to the same
// origin the SPA was served from, so the built app works over the embedded
// service (Option A distribution) and over the Vite dev proxy.

import type {
  ExecutionReport,
  ExecuteRequest,
  ExportResult,
  ScanRequest,
  ScanSummary,
  Session,
  Suggestion,
  SuggestionList,
  ValidateResult,
} from './types';

const BASE = '/api/v1';

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly details: Record<string, unknown> | undefined;

  constructor(status: number, code: string, message: string, details?: Record<string, unknown>) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
    this.details = details;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(BASE + path, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      ...(init?.headers ?? {}),
    },
  });
  if (!res.ok) {
    let code = 'error';
    let message = `HTTP ${res.status}`;
    let details: Record<string, unknown> | undefined;
    try {
      const body = (await res.json()) as { error?: ApiError };
      if (body && typeof body === 'object' && 'error' in body) {
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

export const api = {
  async health(): Promise<void> {
    await request<void>('/health');
  },

  async session(): Promise<Session> {
    return request<Session>('/session');
  },

  async startScan(body: ScanRequest): Promise<void> {
    await request<void>('/session/scan', {
      method: 'POST',
      body: JSON.stringify(body),
    });
  },

  async suggestions(
    params?: { type?: string; kind?: string; status?: string; selected?: boolean },
  ): Promise<SuggestionList> {
    const q = new URLSearchParams();
    if (params?.type) q.set('type', params.type);
    if (params?.kind) q.set('asset_kind', params.kind);
    if (params?.status) q.set('status', params.status);
    if (params?.selected !== undefined) q.set('selected', String(params.selected));
    const qs = q.toString();
    return request<SuggestionList>(`/suggestions${qs ? `?${qs}` : ''}`);
  },

  async selection(): Promise<{ ids: string[] }> {
    return request<{ ids: string[] }>('/selection');
  },

  async selectionPut(ids: string[]): Promise<void> {
    await request<void>('/selection', {
      method: 'PUT',
      body: JSON.stringify({ ids }),
    });
  },

  async selectionAdd(ids: string[]): Promise<void> {
    await request<void>('/selection/add', {
      method: 'POST',
      body: JSON.stringify({ ids }),
    });
  },

  async selectionRemove(ids: string[]): Promise<void> {
    await request<void>('/selection/remove', {
      method: 'POST',
      body: JSON.stringify({ ids }),
    });
  },

  async validate(ids?: string[]): Promise<ValidateResult> {
    return request<ValidateResult>('/validate', {
      method: 'POST',
      body: JSON.stringify({ ids: ids ?? null }),
    });
  },

  async execute(ids: string[]): Promise<void> {
    const body: ExecuteRequest = { ids };
    await request<void>('/execute', {
      method: 'POST',
      body: JSON.stringify(body),
    });
  },

  async cancel(): Promise<void> {
    await request<void>('/cancel', { method: 'POST' });
  },

  async execution(): Promise<ExecutionReport> {
    return request<ExecutionReport>('/execution');
  },

  async summary(): Promise<ScanSummary> {
    return request<ScanSummary>('/report/summary');
  },

  async run(ids: string[]): Promise<void> {
    await this.execute(ids);
  },

  async exportScript(ids: string[]): Promise<ExportResult> {
    const res = await fetch(BASE + '/export', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ids }),
    });
    if (!res.ok) {
      let code = 'error';
      let message = `HTTP ${res.status}`;
      try {
        const body = (await res.json()) as { error?: { code?: string; message?: string } };
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
    const platformHeader = res.headers.get('x-script-platform');
    const countHeader = res.headers.get('x-script-count');
    return {
      platform: platformHeader ?? 'unknown',
      count: countHeader ? Number(countHeader) : 0,
      text,
    };
  },

  async config(): Promise<Record<string, unknown>> {
    return request<Record<string, unknown>>('/config');
  },

  async configValidate(doc: Record<string, unknown>): Promise<ValidateResult> {
    return request<ValidateResult>('/config/validate', {
      method: 'POST',
      body: JSON.stringify(doc),
    });
  },

  async configPut(doc: Record<string, unknown>): Promise<Record<string, unknown>> {
    return request<Record<string, unknown>>('/config', {
      method: 'PUT',
      body: JSON.stringify(doc),
    });
  },

  async cacheClear(): Promise<void> {
    await request<void>('/cache', { method: 'DELETE' });
  },
};

export function resolveApiError(e: unknown): string {
  if (e instanceof ApiError) {
    return `${e.code}: ${e.message}`;
  }
  if (e instanceof Error) {
    return e.message;
  }
  return String(e);
}

export type { Suggestion };