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
  test('highlights the active running stage with session-sweep', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView sessionId="s1" pipeline={runningPipeline} settledText={null} />,
    );
    expect(html).toContain('data-testid="pipeline-stage-0"');
    expect(html).toContain('data-active="true"');
    expect(html).toContain('session-sweep');
    expect(html).toContain('cargo clippy --workspace');
  });

  test('settles onto final output on completion while keeping >=3s stage clickable', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView
        sessionId="s1"
        pipeline={settledPipeline}
        settledText={'\x1b[32m检查完成 完了 완료\x1b[0m\n'}
      />,
    );
    expect(html).toContain('5.8s');
    expect(html).toContain('&lt;1s');
    expect(html).toContain('检查完成 完了 완료');
    expect(html).toContain('#16a34a');
  });

  test('marks retained intermediate stages expired once retention elapses', () => {
    const html = renderToStaticMarkup(
      <PipelineStagesView
        sessionId="s1"
        pipeline={settledPipeline}
        settledText="final result"
        expired
      />,
    );
    expect(html).toContain('5.8s · expired');
    expect(html).toContain('final result');
  });
});
