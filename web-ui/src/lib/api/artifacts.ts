import { apiGet, apiPost, apiSseUrl } from './client';
import type { ApiAttachment } from '../types';
import type {
  ArtifactDiagnosticsResponse,
  SessionArtifactsResponse,
  SlidevPreviewResponse,
} from '../types';

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

/**
 * User-facing artifact page on the daemon. Browsers get the rendered
 * preview page; other clients get the raw file. `profileId` may be `-`
 * when the caller does not know the session's profile.
 */
export function artifactPageUrl(sessionId: string, path: string, profileId = '-'): string {
  const rel = path
    .replace(/^\.?\/?artifacts\//, '')
    .split('/')
    .map(encodeURIComponent)
    .join('/');
  return `/profile/${encodeURIComponent(profileId)}/session/${encodeURIComponent(sessionId)}/artifacts${rel ? `/${rel}` : ''}`;
}

export function artifactDownloadUrl(sessionId: string, path: string, profileId = '-'): string {
  return `${artifactPageUrl(sessionId, path, profileId)}?download`;
}

/** Upload raw bytes for an attachment; returns the record to send with a message. */
export async function uploadAttachment(sessionId: string, file: File): Promise<ApiAttachment> {
  const response = await fetch(
    apiSseUrl(`/sessions/${sessionId}/attachments?name=${encodeURIComponent(file.name)}`),
    { method: 'POST', headers: { 'content-type': file.type || 'application/octet-stream' }, body: file },
  );
  if (!response.ok) {
    throw new Error(`attachment upload failed: ${response.status}`);
  }
  return (await response.json()) as ApiAttachment;
}
