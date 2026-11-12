import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { EffortSlider } from './EffortSlider';

const levels = ['none', 'low', 'medium', 'high', 'xhigh'];

function render(value: number, ladder: string[] = levels) {
  return renderToStaticMarkup(
    <EffortSlider
      levels={ladder}
      value={value}
      labelOf={(level) => level.toUpperCase()}
      onChange={() => {}}
      onCommit={() => {}}
      ariaLabel="Thinking"
    />,
  );
}

describe('EffortSlider', () => {
  test('renders one stop per level of the ladder', () => {
    expect(render(2).match(/data-stop=/g)).toHaveLength(5);
    expect(render(0, ['low', 'high']).match(/data-stop=/g)).toHaveLength(2);
  });

  test('exposes the selected level as an accessible slider', () => {
    const html = render(3);
    expect(html).toContain('role="slider"');
    expect(html).toContain('aria-valuetext="HIGH"');
    expect(html).toContain('aria-valuenow="3"');
    expect(html).toContain('aria-valuemax="4"');
  });
});
