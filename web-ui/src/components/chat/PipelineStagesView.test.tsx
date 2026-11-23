import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { BackgroundPipeline } from '../../lib/api/background';
import { PipelineStagesView } from './PipelineStagesView';

const runningPipeline: BackgroundPipeline = {
  pid: 48210,
  launchId: 9,
  script: 'cargo clippy --workspace | rg error | tail -n 50',
  jobStatus: { state: 'running' },
  elapsedMs: 4100,
  activeStep: 0,
  stages: [
    {
      step: 0,
      command: 'cargo clippy --workspace',
      status: { state: 'running' },
      retained: true,
      uiBytes: 512,
      totalLines: 12,
    },
    {
      step: 1,
      command: 'rg error',
      status: { state: 'pending' },
      retained: false,
      uiBytes: 0,
      totalLines: 0,
    },
    {
      step: 2,
      command: 'tail -n 50',
      status: { state: 'pending' },
      retained: false,
      uiBytes: 0,
      totalLines: 0,
    },
  ],
};

const settledPipeline: BackgroundPipeline = {
  pid: 48210,
  launchId: 9,
  script: 'cargo clippy --workspace | rg error | tail -n 50',
  jobStatus: { state: 'completed', exit_code: 0 },
  elapsedMs: 6200,
  expiresAt: '2026-11-23T03:00:00Z',
  activeStep: null,
  stages: [
    {
      step: 0,
      command: 'cargo clippy --workspace',
      status: { state: 'exited', code: 0 },
      ownDurationMs: 5800,
      retained: true,
      uiBytes: 4096,
      totalLines: 120,
    },
    {
      step: 1,
      command: 'rg error',
      status: { state: 'exited', code: 0 },
      ownDurationMs: 120,
      retained: false,
      uiBytes: 0,
      totalLines: 0,
    },
    {
      step: 2,
      command: 'tail -n 50',
      status: { state: 'exited', code: 0 },
      ownDurationMs: 40,
      retained: false,
      uiBytes: 128,
      totalLines: 3,
    },
  ],
};

describe('PipelineStagesView', () => {
  test('renders normal full command and sweeps only the active stage substring', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView
        sessionId="s1"
        pipeline={runningPipeline}
        script={runningPipeline.script}
        settledText={null}
      />,
    );
    expect(html).toContain('data-testid="pipeline-stage-0"');
    expect(html).toContain('data-active="true"');
    expect(html).toContain('session-sweep');
    expect(html).toContain('cargo clippy --workspace');
    expect(html).toContain('rg error');
    expect(html).toContain('tail -n 50');
    expect(html).not.toContain('[0]');
    expect(html).not.toContain('[1]');
    expect(html).not.toContain('4.1s');
  });

  test('settles onto final colored output without leaking stage timings', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView
        sessionId="s1"
        pipeline={settledPipeline}
        script={settledPipeline.script}
        settledText={'\x1b[32m检查完成 完了 완료\x1b[0m\n'}
      />,
    );
    expect(html).not.toContain('5.8s');
    expect(html).not.toContain('&lt;1s');
    expect(html).not.toContain('[0]');
    expect(html).toContain('data-clickable="true"');
    expect(html).toContain('检查完成 完了 완료');
    expect(html).toContain('#16a34a');
  });

  test('marks retained intermediate stages expired on hover hint once retention elapses', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView
        sessionId="s1"
        pipeline={settledPipeline}
        script={settledPipeline.script}
        settledText="final result"
        expired
      />,
    );
    expect(html).toContain('cargo clippy --workspace (expired)');
    expect(html).not.toContain('5.8s');
    expect(html).toContain('final result');
  });
});
