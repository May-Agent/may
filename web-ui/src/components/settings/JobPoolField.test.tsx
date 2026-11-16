import { beforeEach, describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { JobPoolField } from './JobPoolField';
import { useConfigStore } from '@/lib/stores/config';
import type { UiConfigResponse } from '@/lib/api/config';

function withPoolSize(jobPoolSize: number | null): UiConfigResponse {
  return { fullConfig: { sandbox: { commandPriority: 'normal', jobPoolSize } } } as UiConfigResponse;
}

// Server rendering reads a store's initial state, so the saved config is set there.
function saved(jobPoolSize: number | null) {
  useConfigStore.getInitialState().config = withPoolSize(jobPoolSize);
}

describe('JobPoolField', () => {
  beforeEach(() => saved(null));

  test('an unset pool shows an empty field with the automatic placeholder', () => {
    saved(null);
    const markup = renderToStaticMarkup(<JobPoolField />);
    expect(markup).toContain('id="job-pool-size"');
    expect(markup).toContain('value=""');
    expect(markup).toContain('placeholder="Automatic (CPU cores)"');
  });

  test('a saved pool size fills the field', () => {
    saved(6);
    expect(renderToStaticMarkup(<JobPoolField />)).toContain('value="6"');
  });

  test('a disabled field is rendered disabled', () => {
    saved(6);
    expect(renderToStaticMarkup(<JobPoolField disabled />)).toContain('disabled=""');
  });
});
