import { memo, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { PrismLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import bash from 'react-syntax-highlighter/dist/esm/languages/prism/bash';
import python from 'react-syntax-highlighter/dist/esm/languages/prism/python';
import oneLight from 'react-syntax-highlighter/dist/esm/styles/prism/one-light';
import { X } from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  decodeBase64Utf8,
  readPipelineStage,
  type BackgroundPipeline,
  type PipelineStage,
} from '@/lib/api/background';
import { AnsiOutput } from './AnsiOutput';

SyntaxHighlighter.registerLanguage('bash', bash);
SyntaxHighlighter.registerLanguage('python', python);

const POLL_INTERVAL_MS = 250;

interface PrismNode {
  type: 'element' | 'text';
  value?: string | number;
  properties?: { className?: unknown[] };
  children?: PrismNode[];
}

interface StyledTextSegment {
  text: string;
  style: CSSProperties;
}

interface StageRange {
  stage: PipelineStage;
  start: number;
  end: number;
}

function resolveTokenStyle(
  classNames: string[],
  stylesheet: Record<string, CSSProperties>,
): CSSProperties {
  const nonToken = classNames.filter((name) => name && name !== 'token');
  let merged: CSSProperties = {};
  for (const name of nonToken) {
    const entry = stylesheet[name];
    if (entry) merged = { ...merged, ...entry };
  }
  if (nonToken.length >= 2) {
    const combined = stylesheet[nonToken.join('.')];
    if (combined) merged = { ...merged, ...combined };
  }
  return merged;
}

function flattenPrismNodes(
  nodes: PrismNode[],
  stylesheet: Record<string, CSSProperties>,
  inherited: string[] = [],
  out: StyledTextSegment[] = [],
): StyledTextSegment[] {
  for (const node of nodes) {
    if (node.type === 'text') {
      const value = node.value !== undefined ? String(node.value) : '';
      if (value.length > 0) {
        out.push({
          text: value,
          style: resolveTokenStyle(inherited, stylesheet),
        });
      }
      continue;
    }
    if (node.children) {
      const own = Array.isArray(node.properties?.className)
        ? node.properties.className.filter((c): c is string => typeof c === 'string')
        : [];
      flattenPrismNodes(node.children, stylesheet, [...inherited, ...own], out);
    }
  }
  return out;
}

function resolveStageRanges(script: string, stages: PipelineStage[]): StageRange[] {
  const ranges: StageRange[] = [];
  let cursor = 0;
  for (const stage of stages) {
    const needle = stage.command.trim();
    if (needle.length === 0) continue;
    const rel = script.indexOf(needle, cursor);
    if (rel === -1) continue;
    const start = rel;
    const end = start + needle.length;
    ranges.push({ stage, start, end });
    cursor = end;
  }
  return ranges;
}

type CommandPiece =
  | { kind: 'plain'; segments: StyledTextSegment[] }
  | { kind: 'stage'; stage: PipelineStage; segments: StyledTextSegment[] };

function splitSegmentsByStages(
  segments: StyledTextSegment[],
  ranges: StageRange[],
): CommandPiece[] {
  const pieces: CommandPiece[] = [];
  let cursor = 0;

  const pushPiece = (stage: PipelineStage | null, seg: StyledTextSegment) => {
    const last = pieces[pieces.length - 1];
    if (stage === null) {
      if (last && last.kind === 'plain') {
        last.segments.push(seg);
      } else {
        pieces.push({ kind: 'plain', segments: [seg] });
      }
    } else if (last && last.kind === 'stage' && last.stage.step === stage.step) {
      last.segments.push(seg);
    } else {
      pieces.push({ kind: 'stage', stage, segments: [seg] });
    }
  };

  for (const seg of segments) {
    const segStart = cursor;
    const segEnd = segStart + seg.text.length;
    cursor = segEnd;
    let localAt = 0;
    while (localAt < seg.text.length) {
      const absAt = segStart + localAt;
      const hit = ranges.find((r) => r.start <= absAt && absAt < r.end);
      if (hit) {
        const takeEnd = Math.min(hit.end - segStart, seg.text.length);
        pushPiece(hit.stage, {
          text: seg.text.slice(localAt, takeEnd),
          style: seg.style,
        });
        localAt = takeEnd;
      } else {
        let nextStart = seg.text.length;
        for (const r of ranges) {
          if (r.start > absAt && r.start < segEnd) {
            nextStart = Math.min(nextStart, r.start - segStart);
          }
        }
        pushPiece(null, {
          text: seg.text.slice(localAt, nextStart),
          style: seg.style,
        });
        localAt = nextStart;
      }
    }
  }
  return pieces;
}

const commandHighlighterStyle: CSSProperties = {
  margin: 0,
  padding: 0,
  background: 'transparent',
  fontSize: 'inherit',
  lineHeight: 'inherit',
  flex: 1,
  minWidth: 0,
};

export interface PipelineStagesViewProps {
  sessionId: string | null;
  pipeline: BackgroundPipeline;
  /** Command script text when available from tool arguments. */
  script?: string;
  /** Script interpreter (`shell` or `python`). */
  interpreter?: string | null;
  /** Settled final-stage fallback text from `ToolResult`, when finished. */
  settledText: string | null;
  /** True when the retention window elapsed and intermediate stages were cleared. */
  expired?: boolean;
}

export const PipelineStagesView = memo(function PipelineStagesView({
  sessionId,
  pipeline,
  script,
  interpreter,
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

  const displayedScript = useMemo(() => {
    if (script && script.trim().length > 0) return script;
    if (pipeline.script && pipeline.script.trim().length > 0) return pipeline.script;
    return pipeline.stages.map((s) => s.command.trim()).join(' | ');
  }, [script, pipeline.script, pipeline.stages]);

  const stageRanges = useMemo(
    () => resolveStageRanges(displayedScript, pipeline.stages),
    [displayedScript, pipeline.stages],
  );

  const bufferedText = stageTexts[effectiveStep] ?? '';
  const displayedText =
    !isRunning && effectiveStep === lastStep && bufferedText.length === 0 && settledText !== null
      ? settledText
      : bufferedText;

  const language = interpreter === 'python' ? 'python' : 'bash';
  const selectedStage =
    selectedStep !== null && !expired
      ? pipeline.stages.find((s) => s.step === selectedStep) ?? null
      : null;

  return (
    <div className="space-y-2" data-testid="pipeline-stages">
      <div className="flex gap-2 text-xs font-mono leading-relaxed">
        <span className="shrink-0 text-muted-foreground select-none">$</span>
        <div className="flex-1 min-w-0 max-h-64 overflow-auto scrollbar-thin">
          <SyntaxHighlighter
            language={language}
            style={oneLight}
            customStyle={commandHighlighterStyle}
            codeTagProps={{ style: { background: 'transparent', fontFamily: 'inherit' } }}
            PreTag="div"
            className="whitespace-pre-wrap break-all"
            renderer={({ rows, stylesheet }) => {
              const segments = flattenPrismNodes(
                rows as unknown as PrismNode[],
                stylesheet as Record<string, CSSProperties>,
              );
              const pieces = splitSegmentsByStages(segments, stageRanges);
              return (
                <>
                  {pieces.map((piece, idx) => {
                    if (piece.kind === 'plain') {
                      return piece.segments.map((seg, sIdx) => (
                        <span key={`${idx}-${sIdx}`} style={seg.style}>
                          {seg.text}
                        </span>
                      ));
                    }
                    const { stage } = piece;
                    const active = isRunning && pipeline.activeStep === stage.step;
                    const selected = !expired && selectedStep === stage.step;
                    const isFinal = stage.step === lastStep;
                    const clickable =
                      !expired && (stage.retained || (isFinal && settledText !== null));
                    const title = clickable
                      ? stage.command
                      : `${stage.command} (${expired && !isFinal ? 'expired' : 'not retained'})`;

                    return (
                      <button
                        key={`${idx}-stage-${stage.step}`}
                        type="button"
                        onClick={(e) => {
                          e.stopPropagation();
                          if (!clickable) return;
                          setSelectedStep((prev) => (prev === stage.step ? null : stage.step));
                        }}
                        data-testid={`pipeline-stage-${stage.step}`}
                        data-active={active ? 'true' : 'false'}
                        data-selected={selected ? 'true' : 'false'}
                        data-clickable={clickable ? 'true' : 'false'}
                        title={title}
                        className={cn(
                          'inline text-left rounded-[3px] transition-colors align-baseline',
                          clickable
                            ? 'cursor-pointer hover:bg-muted/80 hover:underline decoration-dotted underline-offset-2'
                            : 'cursor-default hover:bg-muted/40 hover:underline decoration-dotted underline-offset-2',
                          selected && 'bg-primary/15 underline underline-offset-2 ring-1 ring-border/70',
                          active && 'session-sweep font-medium',
                        )}
                      >
                        {piece.segments.map((seg, sIdx) =>
                          active ? (
                            <span key={sIdx}>{seg.text}</span>
                          ) : (
                            <span key={sIdx} style={seg.style}>
                              {seg.text}
                            </span>
                          ),
                        )}
                      </button>
                    );
                  })}
                </>
              );
            }}
          >
            {displayedScript}
          </SyntaxHighlighter>
        </div>
      </div>

      {selectedStage && (
        <div
          className="flex items-center justify-between gap-2 rounded bg-muted/50 px-2 py-0.5 text-xs font-mono text-foreground/90"
          data-testid="pipeline-selected-command"
        >
          <div className="flex items-center gap-1.5 min-w-0">
            <span className="text-primary select-none">▸</span>
            <span className="truncate">{selectedStage.command}</span>
          </div>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              setSelectedStep(null);
            }}
            aria-label="Reset stage selection"
            className="shrink-0 text-muted-foreground hover:text-foreground cursor-pointer"
          >
            <X className="size-3" />
          </button>
        </div>
      )}

      {displayedText.length > 0 && <AnsiOutput text={displayedText} />}
    </div>
  );
});
