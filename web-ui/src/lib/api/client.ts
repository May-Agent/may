import type { ApiErrorCode } from '../types';

export class ApiError extends Error {
  constructor(
    public readonly code: ApiErrorCode | 'network' | 'parse',
    message: string,
    public readonly status?: number,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

interface ApiErrorBody {
  code?: ApiErrorCode;
  message?: string;
}

async function decodeError(response: Response): Promise<ApiError> {
  const status = response.status;
  try {
    const body = (await response.json()) as ApiErrorBody;
    return new ApiError(body.code ?? 'internal_error', body.message ?? response.statusText, status);
  } catch {
    return new ApiError('internal_error', response.statusText, status);
  }
}

export async function apiGet<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, { ...init, method: 'GET' });
  if (!res.ok) throw await decodeError(res);
  return (await res.json()) as T;
}

export async function apiPost<T>(
  path: string,
  body?: unknown,
  init?: RequestInit,
): Promise<T> {
  const headers = new Headers(init?.headers);
  if (body !== undefined && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json');
  }
  const res = await fetch(`/api${path}`, {
    ...init,
    method: 'POST',
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw await decodeError(res);
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (text.length === 0) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    return undefined as T;
  }
}

export async function apiDelete<T>(path: string): Promise<T> {
  const res = await fetch(`/api${path}`, { method: 'DELETE' });
  if (!res.ok) throw await decodeError(res);
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  if (text.length === 0) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    return undefined as T;
  }
}

export function apiSseUrl(path: string): string {
  return `/api${path}`;
}
