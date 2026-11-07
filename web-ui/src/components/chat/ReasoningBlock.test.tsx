import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { ReasoningBlock } from './ReasoningBlock';

const THINKING = [
  '先分析 the request:',
  '',
  '- 第一步 *check* the config',
  '- 次のステップ **verify** 확인',
  '',
  '```rust',
  'let x = 1;',
  '```',
].join('\n');

describe('ReasoningBlock', () => {
  test('renders the thinking body as Markdown elements', () => {
    const markup = renderToStaticMarkup(<ReasoningBlock text={THINKING} streaming />);
    expect(markup).toContain('<li');
    expect(markup).toContain('<em>check</em>');
    expect(markup).toContain('data-streamdown="strong">verify</span>');
    expect(markup).toContain('>let</span>');
    expect(markup).toContain('第一步');
    expect(markup).toContain('次のステップ');
    expect(markup).toContain('확인');
    expect(markup).not.toContain('```');
    expect(markup).not.toContain('**verify**');
  });
});
