import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { TurnToolState } from '../../lib/stream/reducer';
import { ToolCard } from './ToolCard';

function terminalCall(argumentsRaw: string, argumentsError?: string): TurnToolState {
  return {
    id: 'c1',
    name: 'terminal',
    variant: 'terminal',
    argumentsRaw,
    summary: argumentsError === undefined ? {} : { argumentsError },
    status: 'done',
    seq: 0,
  };
}

describe('a tool card with invalid arguments', () => {
  test('shows the marker, the daemon error and the raw arguments', () => {
    const raw = '{"command":"ls"}';
    const markup = renderToStaticMarkup(
      <ToolCard tool={terminalCall(raw, 'missing field `script`')} />,
    );
    expect(markup).toContain('invalid arguments');
    expect(markup).toContain('missing field `script`');
    expect(markup).toContain('{&quot;command&quot;:&quot;ls&quot;}');
  });

  test('a call whose summary carries no error has no marker', () => {
    const markup = renderToStaticMarkup(<ToolCard tool={terminalCall('{"script":"ls"}')} />);
    expect(markup).not.toContain('invalid arguments');
  });
});
