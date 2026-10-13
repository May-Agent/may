import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { Segmented } from './segmented';

const OPTIONS = [
  { value: 'a' as const, label: 'A' },
  { value: 'b' as const, label: 'B' },
];

describe('Segmented', () => {
  test('leaves options enabled by default', () => {
    const markup = renderToStaticMarkup(
      <Segmented value="a" onValueChange={() => {}} options={OPTIONS} />,
    );
    expect(markup).not.toContain('disabled');
  });

  test('disables every option while busy', () => {
    const markup = renderToStaticMarkup(
      <Segmented value="a" onValueChange={() => {}} options={OPTIONS} disabled />,
    );
    expect(markup.match(/disabled=""/g) ?? markup.match(/disabled/g)).toHaveLength(2);
  });
});
