import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { CompactionRow } from './CompactionRow';

const STARTED = {
  kind: 'text',
  text: 'Compaction started in the background. Keep working: the summary replaces the older conversation at a later step.',
};
const REFUSED = {
  kind: 'error',
  message: 'Nothing was started: the context holds fewer than 5000 tokens, too little for compaction to pay off. Carry on with the work.',
};

describe('CompactionRow', () => {
  test('a started compaction is one row with a check and nothing to expand', () => {
    const markup = renderToStaticMarkup(<CompactionRow tool={{ status: 'done', result: STARTED }} />);
    expect(markup).toContain('data-compaction');
    expect(markup).toContain('lucide-check');
    expect(markup).not.toContain('lucide-x');
    expect(markup).not.toContain('<details');
    expect(markup).not.toContain('Compaction started');
  });

  test('a refused compaction is a red cross with the reason, still nothing to expand', () => {
    const markup = renderToStaticMarkup(<CompactionRow tool={{ status: 'done', result: REFUSED }} />);
    expect(markup).toContain('lucide-x');
    expect(markup).toContain('text-destructive');
    expect(markup).toContain('Nothing was started: the context holds fewer than 5000 tokens');
    expect(markup).not.toContain('<details');
  });
});
