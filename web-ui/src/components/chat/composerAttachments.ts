import type { ApiAttachment, AttachmentKind } from '@/lib/types';
import { uploadAttachment } from '@/lib/api/artifacts';
import { isDraftSessionId, useSessionsStore } from '@/lib/stores/sessions';

function detectKind(mime: string): AttachmentKind {
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('audio/')) return 'audio';
  if (mime.startsWith('video/')) return 'video';
  if (mime.startsWith('text/') || mime === 'application/json') return 'text';
  if (mime === 'application/pdf') return 'pdf';
  if (mime.startsWith('application/')) return 'document';
  return 'file';
}

/**
 * The session an attachment upload runs against. A `draft_` session exists
 * only on this client — the daemon 404s any upload to it — so the first
 * upload materializes it (`POST /api/sessions`, exactly like the first
 * send); concurrent and later uploads share that real id via `pending`.
 */
function resolveUploadSession(
  id: string,
  pending: { current: { id: string; session: Promise<string> } | null },
): Promise<string> {
  if (!isDraftSessionId(id)) return Promise.resolve(id);
  if (pending.current?.id === id) return pending.current.session;
  const session = useSessionsStore
    .getState()
    .materialize(id)
    .then((created) => created.id);
  pending.current = { id, session };
  return session;
}

export async function fileToAttachment(
  sessionId: string,
  file: File,
  pending: { current: { id: string; session: Promise<string> } | null },
): Promise<ApiAttachment> {
  const record = await uploadAttachment(await resolveUploadSession(sessionId, pending), file);
  const kind = detectKind(file.type || '');
  if (kind === 'text' && file.size < 256 * 1024) {
    record.textContent = await file.text();
  }
  return record;
}
