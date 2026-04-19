import { AnimatePresence, motion } from 'framer-motion';
import type { Artifact, ArtifactPreviewTarget } from '@/types';
import { ArtifactsSidebar } from './ArtifactsSidebar';
import { BrowserView } from './BrowserView';
import { ContextInspectorPanel } from './ContextInspectorPanel';
import type { PreviewPaneState } from './chatUtils';

const BROWSER_PANEL_WIDTH_PX = 720;
const CONTEXT_PANEL_WIDTH_PX = 480;
const PANEL_ANIMATION_EASE = [0.22, 1, 0.36, 1] as const;
const PANEL_ANIMATION_DURATION = 0.24;

export type RightPanelKind = 'artifacts' | 'browser' | 'context' | null;

interface RightPanelProps {
  active: RightPanelKind;

  // Artifact panel props
  artifacts: Artifact[];
  selectedPreviewPath: string | null;
  previewPane: PreviewPaneState | null;
  showArtifactHistory: boolean;
  onToggleArtifactHistory: () => void;
  onCloseArtifacts: () => void;
  getViewerForPath: (
    rawPath: string,
  ) => ArtifactPreviewTarget['viewer'];
  onSelectArtifact: (target: ArtifactPreviewTarget) => void;

  // Artifact resize props
  artifactPanelWidth: number;
  isArtifactPanelResizing: boolean;
  onArtifactResizerPointerDown: (
    event: React.PointerEvent<HTMLDivElement>,
  ) => void;

  // Browser panel props
  currentSessionId: string | null;
  onCloseBrowser: () => void;

  // Context panel props
  contextPromptTokens?: number;
  contextUsedTokens?: number;
  contextWindow?: number;
  contextUsagePercent: number | null;
  onCloseContext: () => void;
}

/**
 * The right-hand side panel area that hosts either the ArtifactsSidebar
 * (with a draggable resizer) or the BrowserView, mutually exclusive.
 * Wraps each panel in an AnimatePresence motion aside so transitions
 * in/out are consistent across kinds.
 */
export function RightPanel({
  active,
  artifacts,
  selectedPreviewPath,
  previewPane,
  showArtifactHistory,
  onToggleArtifactHistory,
  onCloseArtifacts,
  getViewerForPath,
  onSelectArtifact,
  artifactPanelWidth,
  isArtifactPanelResizing,
  onArtifactResizerPointerDown,
  currentSessionId,
  onCloseBrowser,
  contextPromptTokens,
  contextUsedTokens,
  contextWindow,
  contextUsagePercent,
  onCloseContext,
}: RightPanelProps) {
  return (
    <AnimatePresence initial={false} mode="wait">
      {active === 'artifacts' && (
        <>
          <div
            role="separator"
            aria-orientation="vertical"
            aria-label="Resize artifacts panel"
            className={`group relative h-full w-2 flex-shrink-0 touch-none cursor-col-resize ${
              isArtifactPanelResizing ? 'bg-primary/10' : ''
            }`}
            onPointerDown={onArtifactResizerPointerDown}
          >
            <div
              className={`absolute inset-y-0 left-1/2 w-px -translate-x-1/2 ${
                isArtifactPanelResizing
                  ? 'bg-primary'
                  : 'bg-border/80 group-hover:bg-primary/70'
              }`}
            />
          </div>
          <motion.aside
            key="artifacts-panel"
            initial={{ width: 0, opacity: 0 }}
            animate={{ width: artifactPanelWidth, opacity: 1 }}
            exit={{ width: 0, opacity: 0 }}
            transition={{
              duration: PANEL_ANIMATION_DURATION,
              ease: PANEL_ANIMATION_EASE,
            }}
            className="h-full flex-shrink-0 overflow-hidden"
          >
            <ArtifactsSidebar
              open
              artifacts={artifacts}
              selectedPath={selectedPreviewPath}
              preview={previewPane}
              showHistory={showArtifactHistory}
              onToggleHistory={onToggleArtifactHistory}
              onClose={onCloseArtifacts}
              getViewerForPath={getViewerForPath}
              onSelect={onSelectArtifact}
            />
          </motion.aside>
        </>
      )}

      {active === 'browser' && (
        <motion.aside
          key="browser-panel"
          initial={{ width: 0, opacity: 0 }}
          animate={{ width: BROWSER_PANEL_WIDTH_PX, opacity: 1 }}
          exit={{ width: 0, opacity: 0 }}
          transition={{
            duration: PANEL_ANIMATION_DURATION,
            ease: PANEL_ANIMATION_EASE,
          }}
          className="h-full flex-shrink-0 overflow-hidden"
        >
          <BrowserView
            open
            onClose={onCloseBrowser}
            sessionId={currentSessionId}
          />
        </motion.aside>
      )}

      {active === 'context' && (
        <motion.aside
          key="context-panel"
          initial={{ width: 0, opacity: 0 }}
          animate={{ width: CONTEXT_PANEL_WIDTH_PX, opacity: 1 }}
          exit={{ width: 0, opacity: 0 }}
          transition={{
            duration: PANEL_ANIMATION_DURATION,
            ease: PANEL_ANIMATION_EASE,
          }}
          className="h-full flex-shrink-0 overflow-hidden"
        >
          <ContextInspectorPanel
            currentSessionId={currentSessionId}
            promptTokens={contextPromptTokens}
            contextUsedTokens={contextUsedTokens}
            contextWindow={contextWindow}
            contextUsagePercent={contextUsagePercent}
            onClose={onCloseContext}
          />
        </motion.aside>
      )}
    </AnimatePresence>
  );
}
