import type { Dispatch, SetStateAction } from 'react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  fetchArtifactPreview,
  fetchSessionArtifacts,
  mapContainerLocalArtifactUrl,
  prepareSlidevPreview,
  subscribeSessionArtifacts,
} from '@/lib/api';
import type {
  Artifact,
  ArtifactPreviewTarget,
  Message,
  ProfileInfo,
} from '@/types';
import {
  isPreviewUrl,
  normalizeArtifactPath,
  normalizePreviewUrl,
} from '@/lib/artifactPath';
import {
  buildArtifactViewerHints,
  createDisconnectBanner,
  type PreviewPaneState,
} from '@/components/chat/chatUtils';

export interface UseArtifactPanelInput {
  currentSessionId: string | null;
  currentProfile: ProfileInfo | null;
  messages: Message[];
  /** Receives user-facing artifact panel errors (matches React.useState setter shape). */
  setError: Dispatch<SetStateAction<string | null>>;
}

export interface UseArtifactPanelResult {
  /** Known artifacts for the current session. */
  artifacts: Artifact[];
  /** Whether the artifacts panel is visible. */
  isOpen: boolean;
  setIsOpen: (open: boolean) => void;
  /** Whether the artifact history drawer is expanded. */
  showHistory: boolean;
  toggleHistory: () => void;
  /** Currently-selected preview path (or null). */
  selectedPreviewPath: string | null;
  /** Viewer hint for the current selection (slidev / default / undefined). */
  selectedPreviewViewer: ArtifactPreviewTarget['viewer'];
  /** Fully-resolved preview pane descriptor (or null when nothing to show). */
  previewPane: PreviewPaneState | null;
  /** Resolve a raw path into the ArtifactViewerKind the UI should use. */
  getViewerForPath: (rawPath: string) => ArtifactPreviewTarget['viewer'];
  /** Open a preview by path or full target descriptor. */
  openPreview: (target: ArtifactPreviewTarget | string) => Promise<void>;
  /** Imperatively refresh the artifact list for the current session. */
  refreshArtifacts: () => Promise<Artifact[]>;
}

/**
 * Owns the artifact panel state machine: artifact list, preview pane,
 * selected path, viewer hints, blob URL lifecycle, and the SSE
 * subscription that keeps the list fresh.
 *
 * Pulled out of ChatInterface so the main chat component no longer
 * manages any artifact lifecycle details — it just reads the
 * returned values and wires them into the ArtifactsSidebar.
 */
export function useArtifactPanel({
  currentSessionId,
  currentProfile,
  messages,
  setError,
}: UseArtifactPanelInput): UseArtifactPanelResult {
  const onError = useCallback(
    (message: string) => setError(message),
    [setError],
  );
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [isOpen, setIsOpen] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  const [selectedPreviewPath, setSelectedPreviewPath] = useState<string | null>(
    null,
  );
  const [selectedPreviewViewer, setSelectedPreviewViewer] =
    useState<ArtifactPreviewTarget['viewer']>(undefined);
  const [previewPane, setPreviewPane] = useState<PreviewPaneState | null>(null);

  const previewBlobUrlRef = useRef<string | null>(null);
  const selectedPreviewPathRef = useRef<string | null>(null);
  const selectedPreviewViewerRef =
    useRef<ArtifactPreviewTarget['viewer']>(undefined);
  const previewSessionRef = useRef<string | null>(null);

  // Keep refs in sync with state so async callbacks read the latest value.
  useEffect(() => {
    selectedPreviewPathRef.current = selectedPreviewPath;
  }, [selectedPreviewPath]);

  useEffect(() => {
    selectedPreviewViewerRef.current = selectedPreviewViewer;
  }, [selectedPreviewViewer]);

  // ── Viewer hints derived from message content ──────────────

  const artifactViewerHints = useMemo(
    () => buildArtifactViewerHints(messages, normalizeArtifactPath),
    [messages],
  );

  const getViewerForPath = useCallback(
    (rawPath: string): ArtifactPreviewTarget['viewer'] => {
      const normalized = normalizeArtifactPath(rawPath);
      if (!normalized) return undefined;
      return artifactViewerHints.get(normalized);
    },
    [artifactViewerHints],
  );

  const pickPreferredArtifactTarget = useCallback(
    (nextArtifacts: Artifact[]): ArtifactPreviewTarget | null => {
      if (nextArtifacts.length === 0) return null;
      const artifactPaths = new Set(
        nextArtifacts.map((artifact) => artifact.path),
      );
      const hintedEntries = Array.from(artifactViewerHints.entries());
      for (let index = hintedEntries.length - 1; index >= 0; index -= 1) {
        const [path, viewer] = hintedEntries[index];
        if (artifactPaths.has(path)) {
          return { path, viewer };
        }
      }
      const fallbackPath = nextArtifacts[0].path;
      return { path: fallbackPath, viewer: getViewerForPath(fallbackPath) };
    },
    [artifactViewerHints, getViewerForPath],
  );

  // ── Refresh artifacts ──────────────────────────────────────

  const refreshArtifacts = useCallback(async (): Promise<Artifact[]> => {
    if (!currentSessionId) {
      setArtifacts([]);
      return [];
    }
    try {
      const next = await fetchSessionArtifacts(currentSessionId);
      setArtifacts(next);
      if (next.length === 0) {
        // Keep the preview pane visible so it doesn't flicker while the
        // artifacts index catches up with filesystem changes.
        return next;
      }

      const currentPreviewPath = selectedPreviewPathRef.current;
      if (currentPreviewPath && !isPreviewUrl(currentPreviewPath)) {
        const selectedArtifactPath = normalizeArtifactPath(currentPreviewPath);
        const exists = next.some((a) => a.path === selectedArtifactPath);
        if (!exists) {
          const fallbackTarget = pickPreferredArtifactTarget(next);
          if (!fallbackTarget) return next;
          setSelectedPreviewPath(fallbackTarget.path);
          setSelectedPreviewViewer(fallbackTarget.viewer);
          selectedPreviewPathRef.current = fallbackTarget.path;
          selectedPreviewViewerRef.current = fallbackTarget.viewer;
          setIsOpen(true);
        } else {
          const hintedViewer = getViewerForPath(selectedArtifactPath);
          if (selectedPreviewViewerRef.current !== hintedViewer) {
            setSelectedPreviewViewer(hintedViewer);
            selectedPreviewViewerRef.current = hintedViewer;
          }
        }
      } else if (!currentPreviewPath) {
        const fallbackTarget = pickPreferredArtifactTarget(next);
        if (fallbackTarget) {
          setSelectedPreviewPath(fallbackTarget.path);
          setSelectedPreviewViewer(fallbackTarget.viewer);
          selectedPreviewPathRef.current = fallbackTarget.path;
          selectedPreviewViewerRef.current = fallbackTarget.viewer;
          setIsOpen(true);
        }
      }
      return next;
    } catch (error) {
      onError(
        `Failed to load artifacts: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
      return [];
    }
  }, [
    currentSessionId,
    getViewerForPath,
    onError,
    pickPreferredArtifactTarget,
  ]);

  // ── Load and display a single preview ──────────────────────

  const openPreview = useCallback(
    async (target: ArtifactPreviewTarget | string) => {
      const request = typeof target === 'string' ? { path: target } : target;
      const rawPath = request.path.trim();
      if (!rawPath || !currentSessionId) return;

      const normalizedUrl = normalizePreviewUrl(rawPath);
      const previewPath = normalizedUrl ?? normalizeArtifactPath(rawPath);
      if (!previewPath) {
        onError('artifact path is required');
        setPreviewPane({
          kind: 'text',
          text: 'Preview failed: artifact path is required',
        });
        return;
      }

      setSelectedPreviewPath(previewPath);
      setSelectedPreviewViewer(request.viewer);
      selectedPreviewPathRef.current = previewPath;
      selectedPreviewViewerRef.current = request.viewer;
      setIsOpen(true);
      setShowHistory(false);

      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }

      if (normalizedUrl) {
        await openExternalUrlPreview(
          normalizedUrl,
          request.viewer,
          currentSessionId,
          currentProfile,
          setPreviewPane,
        );
        return;
      }

      try {
        await openArtifactBytesPreview(
          previewPath,
          request.viewer,
          currentSessionId,
          previewBlobUrlRef,
          setPreviewPane,
        );
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        onError(`Artifact preview failed: ${reason}`);
        setPreviewPane({ kind: 'text', text: `Preview failed: ${reason}` });
      }
    },
    [currentProfile, currentSessionId, onError],
  );

  // ── Session switch: reset panel state ──────────────────────

  useEffect(() => {
    const nextSessionId = currentSessionId ?? null;
    const sessionChanged = previewSessionRef.current !== nextSessionId;
    if (sessionChanged) {
      previewSessionRef.current = nextSessionId;
      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }
      setArtifacts([]);
      setShowHistory(false);
      setSelectedPreviewPath(null);
      setSelectedPreviewViewer(undefined);
      selectedPreviewPathRef.current = null;
      selectedPreviewViewerRef.current = undefined;
      setPreviewPane(null);
      setIsOpen(false);
    }

    if (!currentSessionId) return;

    void refreshArtifacts();
    const banner = createDisconnectBanner(
      setError,
      'Artifacts stream disconnected; reconnecting...',
    );

    const unsubscribe = subscribeSessionArtifacts(currentSessionId, {
      onArtifacts: () => {
        banner.clear();
        void (async () => {
          const nextArtifacts = await refreshArtifacts();
          const currentPreviewPath = selectedPreviewPathRef.current;
          if (nextArtifacts.length === 0 || !currentPreviewPath) return;
          if (isPreviewUrl(currentPreviewPath)) return;
          const stillExists = nextArtifacts.some(
            (artifact) => artifact.path === currentPreviewPath,
          );
          if (!stillExists) return;
          await openPreview({
            path: currentPreviewPath,
            viewer: selectedPreviewViewerRef.current,
          });
        })();
      },
      onOpen: () => banner.clear(),
      onHeartbeat: () => banner.clear(),
      onError: () => banner.schedule(),
    });

    return () => {
      banner.cleanup();
      unsubscribe();
    };
  }, [currentSessionId, setError, openPreview, refreshArtifacts]);

  // If the selected path is an artifact path but we don't have a preview
  // yet (e.g. session reloaded from disk), open it once the artifact list
  // catches up.
  useEffect(() => {
    if (!currentSessionId || !selectedPreviewPath || previewPane !== null) {
      return;
    }
    if (!isPreviewUrl(selectedPreviewPath)) {
      const exists = artifacts.some(
        (artifact) => artifact.path === selectedPreviewPath,
      );
      if (!exists) return;
    }
    void openPreview({
      path: selectedPreviewPath,
      viewer: selectedPreviewViewer,
    });
  }, [
    artifacts,
    currentSessionId,
    openPreview,
    previewPane,
    selectedPreviewPath,
    selectedPreviewViewer,
  ]);

  const toggleHistory = useCallback(() => {
    setShowHistory((prev) => !prev);
  }, []);

  return {
    artifacts,
    isOpen,
    setIsOpen,
    showHistory,
    toggleHistory,
    selectedPreviewPath,
    selectedPreviewViewer,
    previewPane,
    getViewerForPath,
    openPreview,
    refreshArtifacts,
  };
}

// ── Preview loading helpers ──────────────────────────────────

/**
 * Resolve an external URL target into a preview-pane descriptor.
 * Container profiles route through the backend's local-url-map so
 * the frontend never has to duplicate localhost detection logic.
 */
async function openExternalUrlPreview(
  normalizedUrl: string,
  viewer: ArtifactPreviewTarget['viewer'],
  sessionId: string,
  currentProfile: ProfileInfo | null,
  setPreviewPane: (pane: PreviewPaneState | null) => void,
): Promise<void> {
  let resolvedUrl = normalizedUrl;
  const isContainerProfile = currentProfile?.environment.type === 'container';
  if (isContainerProfile) {
    try {
      resolvedUrl = await mapContainerLocalArtifactUrl(sessionId, normalizedUrl);
    } catch {
      // Backend rejected the URL (not a mappable local host, or missing
      // port, etc.). Fall through to show the original URL directly.
    }
  }

  setPreviewPane({
    kind: viewer === 'slidev' ? 'slidev' : 'external',
    url: resolvedUrl,
  });
}

/**
 * Fetch the raw artifact bytes and convert them into the appropriate
 * preview-pane descriptor based on content type and file extension.
 */
async function openArtifactBytesPreview(
  previewPath: string,
  viewer: ArtifactPreviewTarget['viewer'],
  sessionId: string,
  previewBlobUrlRef: React.MutableRefObject<string | null>,
  setPreviewPane: (pane: PreviewPaneState | null) => void,
): Promise<void> {
  if (viewer === 'slidev') {
    const previewUrl = await prepareSlidevPreview(sessionId, previewPath);
    setPreviewPane({ kind: 'slidev', url: previewUrl });
    return;
  }

  const response = await fetchArtifactPreview(sessionId, previewPath);
  const contentType = (response.headers.get('content-type') || '').toLowerCase();
  const ext = previewPath.split('.').pop()?.toLowerCase() || '';
  const arrayBuffer = await response.arrayBuffer();
  const bytes = new Uint8Array(arrayBuffer);

  const isTextual =
    contentType.startsWith('text/') ||
    contentType.includes('json') ||
    contentType.includes('xml') ||
    ['md', 'markdown', 'txt', 'log', 'csv'].includes(ext);
  const text = isTextual ? new TextDecoder().decode(bytes) : '';

  const isMarkdown =
    contentType.includes('markdown') || ['md', 'markdown'].includes(ext);
  if (isMarkdown) {
    setPreviewPane({ kind: 'markdown', markdown: text });
    return;
  }
  if (
    contentType.startsWith('text/plain') ||
    ['txt', 'log', 'csv', 'json', 'xml'].includes(ext)
  ) {
    setPreviewPane({ kind: 'text', text });
    return;
  }
  if (
    ext === 'docx' ||
    contentType.includes('officedocument.wordprocessingml.document')
  ) {
    const mammoth = await import('mammoth');
    const htmlResult = await mammoth.default.convertToHtml({ arrayBuffer });
    const htmlBlob = new Blob([htmlResult.value], {
      type: 'text/html;charset=utf-8',
    });
    const htmlUrl = URL.createObjectURL(htmlBlob);
    previewBlobUrlRef.current = htmlUrl;
    setPreviewPane({ kind: 'iframe', url: htmlUrl });
    return;
  }

  const blob = new Blob([bytes], {
    type: response.headers.get('content-type') || 'application/octet-stream',
  });
  const objectUrl = URL.createObjectURL(blob);
  previewBlobUrlRef.current = objectUrl;

  if (contentType.startsWith('image/')) {
    setPreviewPane({ kind: 'image', url: objectUrl });
  } else if (contentType.startsWith('audio/')) {
    setPreviewPane({ kind: 'audio', url: objectUrl });
  } else {
    setPreviewPane({ kind: 'iframe', url: objectUrl });
  }
}
