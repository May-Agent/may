import { describe, expect, test } from 'bun:test';
import type { TurnToolState } from '../../lib/stream/reducer';
import type { ToolSummary } from '../../lib/types';

function firstLine(text: string): string {
  const nl = text.indexOf('\n');
  return nl === -1 ? text : text.slice(0, nl);
}

// Mirror of `ToolCard.tsx::pickPrimaryLabel` terminal branch. The frontend
// reads tool.summary directly (backend projects it via partial-json-fixer).
// This test MUST fail if anyone flips script before description — that swap
// silently broke the entire summary chip (regression seen 2026-04).
function pickTerminalLabel(tool: TurnToolState, fallback: string): string {
  const s = tool.summary ?? {};
  if (s.description) return s.description;
  if (s.script) return firstLine(s.script);
  return fallback;
}

function fakeTool(summary: ToolSummary): TurnToolState {
  return {
    id: 'c1',
    name: 'terminal',
    variant: 'terminal',
    argumentsRaw: '',
    summary,
    status: 'streaming',
    seq: 0,
  };
}

describe('pickPrimaryLabel — terminal variant', () => {
  test('description wins once present', () => {
    const t = fakeTool({ description: 'Check time', script: 'date' });
    expect(pickTerminalLabel(t, 'Running')).toBe('Check time');
  });

  test('description keeps winning across the streaming lifetime', () => {
    // Frames the BACKEND would emit as the summary projection grows.
    // Each frame represents tool.summary at that point in the stream.
    const frames: ToolSummary[] = [
      {},
      { description: 'Get' },
      { description: 'Get current time' },
      { description: 'Get current time', script: 'date' },
      { description: 'Get current time', script: 'date +%s' },
    ];
    const labels = frames.map((summary) =>
      pickTerminalLabel(fakeTool(summary), 'Running a task'),
    );
    expect(labels[0]).toBe('Running a task');
    expect(labels[1]).toBe('Get');
    expect(labels[2]).toBe('Get current time');
    expect(labels[3]).toBe('Get current time');
    expect(labels[4]).toBe('Get current time');
    // No frame is allowed to surface the raw script as a label.
    expect(labels).not.toContain('date');
    expect(labels).not.toContain('date +%s');
  });

  test('falls back to first line of script when description is absent', () => {
    const t = fakeTool({ script: 'set -e\ndate +%s' });
    expect(pickTerminalLabel(t, 'Running')).toBe('set -e');
  });

  test('falls back to variant label when summary is empty', () => {
    const t = fakeTool({});
    expect(pickTerminalLabel(t, 'Running a task')).toBe('Running a task');
  });
});
