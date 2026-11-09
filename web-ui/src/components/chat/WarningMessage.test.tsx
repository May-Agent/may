import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { WarningMessage } from './WarningMessage';

describe('WarningMessage', () => {
  test('shows the text with a warning icon in the warning colour', () => {
    const html = renderToStaticMarkup(<WarningMessage message="CI / test ran 52m 10s" />);
    expect(html).toContain('CI / test ran 52m 10s');
    expect(html).toContain('<svg');
    expect(html).toContain('text-amber-600');
  });
});
