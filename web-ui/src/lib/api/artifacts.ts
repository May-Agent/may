import { apiGet, apiPost, apiSseUrl } from './client';
import type {
  ArtifactDiagnosticsResponse,
  ArtifactStreamPayload,
  SessionArtifactsResponse,
  SlidevPreviewResponse,
} from '../types';
import { parseNamedSseStream } from '../stream/sse';

export function listArtifacts(sessionId: string): Promise<SessionArtifactsResponse> {
  return apiGet<SessionArtifactsResponse>(
    `/sessions/${encodeURIComponent(sessionId)}/artifacts`,
  );
}

export function artifactPreviewUrl(sessionId: string, path: string): string {
  const qs = new URLSearchParams({ path });
  return `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/preview?${qs.toString()}`;
}

export function artifactRuntimePreviewUrl(
  sessionId: string,
  path: string,
  artifactType: string,
): string {
  const qs = new URLSearchParams({ path, artifactType });
  return `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/runtime/preview?${qs.toString()}`;
}

export function outputsPreviewUrl(sessionId: string, path: string): string {
  const qs = new URLSearchParams({ path });
  return `/api/sessions/${encodeURIComponent(sessionId)}/outputs/preview?${qs.toString()}`;
}

export function prepareSlidevPreview(
  sessionId: string,
  path: string,
): Promise<SlidevPreviewResponse> {
  const qs = new URLSearchParams({ path });
  return apiGet<SlidevPreviewResponse>(
    `/sessions/${encodeURIComponent(sessionId)}/artifacts/slidev/preview?${qs.toString()}`,
  );
}

export function reportArtifactDiagnostics(
  sessionId: string,
  path: string,
  artifactType: string,
  diagnostics: string[],
): Promise<ArtifactDiagnosticsResponse> {
  return apiPost<ArtifactDiagnosticsResponse>(
    `/sessions/${encodeURIComponent(sessionId)}/artifacts/diagnostics`,
    { path, artifactType, diagnostics },
  );
}

export type ArtifactStreamEvent =
  | { kind: 'artifacts'; payload: ArtifactStreamPayload }
  | { kind: 'heartbeat' };

export interface ArtifactStreamHandle {
  events: AsyncGenerator<ArtifactStreamEvent, void, void>;
  abort: () => void;
}

export async function openArtifactStream(
  sessionId: string,
  abort?: AbortSignal,
): Promise<ArtifactStreamHandle> {
  const controller = new AbortController();
  if (abort) {
    if (abort.aborted) controller.abort();
    else abort.addEventListener('abort', () => controller.abort(), { once: true });
  }
  const res = await fetch(
    apiSseUrl(`/sessions/${encodeURIComponent(sessionId)}/artifacts/stream`),
    { headers: { Accept: 'text/event-stream' }, signal: controller.signal },
  );
  if (!res.ok) throw new Error(`artifacts stream failed: ${res.status}`);

  const raw = parseNamedSseStream<ArtifactStreamPayload>(res, controller.signal);

  async function* translate(): AsyncGenerator<ArtifactStreamEvent, void, void> {
    for await (const evt of raw) {
      if (evt.event === 'artifacts' && evt.data) {
        yield { kind: 'artifacts', payload: evt.data };
      } else if (evt.event === 'heartbeat') {
        yield { kind: 'heartbeat' };
      }
    }
  }

  return { events: translate(), abort: () => controller.abort() };
}
