import { apiGet, apiSseUrl } from './client';
import type { SessionArtifactsResponse, ArtifactStreamPayload } from '../types';
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

export function outputsPreviewUrl(sessionId: string, path: string): string {
  const qs = new URLSearchParams({ path });
  return `/api/sessions/${encodeURIComponent(sessionId)}/outputs/preview?${qs.toString()}`;
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
