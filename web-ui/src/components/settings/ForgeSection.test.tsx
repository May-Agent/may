import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { ForgeSection, HostCard } from './ForgeSection';

describe('ForgeSection', () => {
  test('a configured host shows its name, kind and connected status', () => {
    const html = renderToStaticMarkup(
      <HostCard
        host="github.com"
        kind="github"
        configured
        mode="token"
        client_id={null}
        authenticated
        onChanged={() => {}}
      />,
    );
    expect(html).toContain('github.com');
    expect(html).toContain('connected');
    expect(html).toContain('Personal access token');
  });

  test('an unconfigured host offers the login modes and no disconnect', () => {
    const html = renderToStaticMarkup(
      <HostCard
        host="gitlab.com"
        kind="gitlab"
        configured={false}
        mode={null}
        client_id={null}
        authenticated={false}
        onChanged={() => {}}
      />,
    );
    expect(html).toContain('gitlab.com');
    expect(html).toContain('not signed in');
    expect(html).toContain('OAuth');
    expect(html).not.toContain('Disconnect');
  });

  test('the section renders its header while hosts load', () => {
    const html = renderToStaticMarkup(<ForgeSection />);
    expect(html).toContain('Forge');
  });
});
