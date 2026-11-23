import { memo, useEffect, useRef, useState } from 'react';
import { cn } from '@/lib/utils';
import {
  decodeBase64Utf8,
  readPipelineStage,
  type BackgroundPipeline,
  type PipelineStage,
} from '@/lib/api/background';
import { AnsiOutput } from './AnsiOutput';

const POLL_INTERVAL_MS = 250;

function formatStageDuration(ms: number | undefined): string | null {
  if (ms === undefined) return null;
  if (ms >= 60_000) {
    const mins = Math.floor(ms / 60_000);
    const secs = Math.floor((ms % 60_000) / 1000);
    return `${mins}m${secs.toString().padStart(2, '0')}s`;
  }
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)}s`;
  }
  return '<1s';
}

function stageBadge(stage: PipelineStage, expired: boolean): string | null {
  if (stage.status.state === 'running') return 'running';
  if (stage.status.state === 'pending') return null;
  const dur = formatStageDuration(stage.ownDurationMs);
  if (expired && stage.retained) return dur ? `${dur} · expired` : 'expired';
  if (!stage.retained) return dur ?? '<3s';
  if (stage.status.code !== 0) {
    return dur ? `exit ${stage.status.code} · ${dur}` : `exit ${stage.status.code}`;
  }
  return dur;
}

export interface PipelineStagesViewProps {
  sessionId: string | null;
  pipeline: BackgroundPipeline;
  /** Settled final-stage fallback text from `ToolResult`, when finished. */
  settledText: string | null;
  /** True when the retention window elapsed and intermediate stages were cleared. */
  expired?: boolean;
}

export const PipelineStagesView = memo(function PipelineStagesView({
  sessionId,
  pipeline,
  settledText,
  expired = false,
}: PipelineStagesViewProps) {
  const [selectedStep, setSelectedStep] = useState<number | null>(null);
  const [stageTexts, setStageTexts] = useState<Record<number, string>>({});
  const cursorsRef = useRef<Record<number, number>>({});

  const lastStep = pipeline.stages.length > 0 ? pipeline.stages[pipeline.stages.length - 1].step : 0;
  const isRunning = pipeline.jobStatus.state === 'running';
  // Automatic follow rule: follow `activeStep` while running; once finished,
  // settle naturally onto the final stage unless the user clicked a retained stage.
  const effectiveStep =
    selectedStep !== null && !expired
      ? selectedStep
      : isRunning
        ? (pipeline.activeStep ?? 0)
        : lastStep;

  const targetStage = pipeline.stages.find((s) => s.step === effectiveStep);
  const shouldFetchStage =
    Boolean(sessionId) &&
    !expired &&
    Boolean(targetStage?.retained) &&
    (isRunning || selectedStep !== null || settledText === null);

  useEffect(() => {
    if (!sessionId || !shouldFetchStage) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const pump = async () => {
      if (cancelled) return;
      const current = cursorsRef.current[effectiveStep] ?? 0;
      try {
        const delta = await readPipelineStage(
          sessionId,
          pipeline.pid,
          effectiveStep,
          current,
          pipeline.launchId,
        );
        if (cancelled) return;
        if (delta.bytes.length > 0 || delta.cursor !== current) {
          const chunk = decodeBase64Utf8(delta.bytes);
          if (cursorsRef.current[effectiveStep] === current || current === 0) {
            cursorsRef.current[effectiveStep] = delta.cursor;
            if (chunk.length > 0) {
              setStageTexts((prev) => ({
                ...prev,
                [effectiveStep]: (prev[effectiveStep] ?? '') + chunk,
              }));
            }
          }
        }
      } catch {
        // Stage expired or ended between snapshots; next SSE delta updates state.
      }
      if (!cancelled && isRunning) {
        timer = setTimeout(() => void pump(), POLL_INTERVAL_MS);
      }
    };

    void pump();
    return () => {
      cancelled = true;
      if (timer !== null) clearTimeout(timer);
    };
  }, [sessionId, pipeline.pid, pipeline.launchId, effectiveStep, isRunning, shouldFetchStage]);

  const bufferedText = stageTexts[effectiveStep] ?? '';
  const displayedText =
    !isRunning && effectiveStep === lastStep && bufferedText.length === 0 && settledText !== null
      ? settledText
      : bufferedText;

  return (
    <div className="space-y-2" data-testid="pipeline-stages">
      <div className="flex flex-wrap items-center gap-1 text-xs font-mono">
        {pipeline.stages.map((stage, idx) => {
          const active = isRunning && pipeline.activeStep === stage.step;
          const selected = effectiveStep === stage.step;
          const isFinal = stage.step === lastStep;
          const clickable = !expired && (stage.retained || (isFinal && settledText !== null));
          const badge = stageBadge(stage, expired && !isFinal);

          return (
            <div key={stage.step} className="flex items-center gap-1 min-w-0">
              {idx > 0 && <span className="text-muted-foreground select-none">|</span>}
              <button
                type="button"
                disabled={!clickable}
                onClick={() => {
                  if (!clickable) return;
                  setSelectedStep((prev) =>
                    prev === stage.step || (!isRunning && isFinal) ? null : stage.step,
                  );
                }}
                data-testid={`pipeline-stage-${stage.step}`}
                data-active={active ? 'true' : 'false'}
                data-selected={selected ? 'true' : 'false'}
                className={cn(
                  'inline-flex items-center gap-1.5 rounded px-1.5 py-0.5 text-left transition-colors max-w-full',
                  selected
                    ? 'bg-muted/80 text-foreground ring-1 ring-border'
                    : clickable
                      ? 'hover:bg-muted/40 text-foreground/80 cursor-pointer'
                      : 'text-muted-foreground/55 cursor-default',
                )}
              >
                <span className="text-[10px] text-muted-foreground select-none">
                  [{stage.step}]
                </span>
                <span
                  className={cn(
                    'truncate max-w-[24rem]',
                    active && 'session-sweep font-medium',
                  )}
                  title={stage.command}
                >
                  {stage.command}
                </span>
                {badge && (
                  <span
                    className={cn(
                      'text-[10px] shrink-0',
                      stage.status.state === 'exited' && stage.status.code !== 0
                        ? 'text-destructive'
                        : 'text-muted-foreground',
                    )}
                  >
                    {badge}
                  </span>
                )}
              </button>
            </div>
          );
        })}
      </div>

      {displayedText.length > 0 && <AnsiOutput text={displayedText} />}
    </div>
  );
});
