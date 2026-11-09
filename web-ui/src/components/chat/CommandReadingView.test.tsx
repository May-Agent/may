import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { CommandReading, PersistedPermissionRequest } from '../../lib/types';
import { PermissionBody } from '../interactions/PermissionBody';
import { recognisesAny } from '../../lib/commandReading';
import { CommandReadingView } from './CommandReadingView';

// Readings as the daemon serializes them (`command_reading` in the Rust crate).
const edit: CommandReading = {
  steps: [
    {
      link: 'start',
      line: 'Edit 2 files',
      intent: {
        kind: 'edit',
        files: [
          {
            path: 'src/a.rs',
            change: {
              kind: 'update',
              hunks: [
                {
                  anchor: 'fn main',
                  lines: [
                    { op: 'context', text: 'keep', old: 1, new: 1 },
                    { op: 'removed', text: 'old line', old: 2 },
                    { op: 'added', text: 'new line', new: 2 },
                  ],
                },
              ],
            },
          },
          {
            path: 'b.txt',
            change: { kind: 'add', lines: [{ op: 'added', text: 'one', new: 1 }] },
          },
        ],
      },
    },
  ],
};

const read: CommandReading = {
  steps: [
    {
      link: 'start',
      line: 'Read src/lib.rs',
      intent: { kind: 'read', path: 'src/lib.rs', range: { kind: 'whole' } },
    },
  ],
};

const search: CommandReading = {
  steps: [
    {
      link: 'start',
      line: 'Search for "fn main" in src',
      intent: { kind: 'search', tool: 'rg', pattern: 'fn main', paths: ['src'] },
    },
  ],
};

const mixed: CommandReading = {
  steps: [
    ...search.steps,
    { link: 'pipe', line: 'wc -l', intent: { kind: 'unknown', source: 'wc -l' } },
    {
      link: 'and',
      line: 'Run cargo test',
      intent: { kind: 'build', tool: 'cargo', subcommand: 'test' },
    },
  ],
};

const unrecognised: CommandReading = {
  steps: [{ link: 'start', line: 'echo hi', intent: { kind: 'unknown', source: 'echo hi' } }],
};

const html = (reading: CommandReading) => renderToStaticMarkup(<CommandReadingView reading={reading} />);

describe('CommandReadingView', () => {
  test('an edit shows each file with a line-numbered diff', () => {
    const markup = html(edit);
    expect(markup).toContain('~ src/a.rs');
    expect(markup).toContain('+ b.txt (new file)');
    expect(markup).toContain('@@ fn main');
    expect(markup).toContain('old line');
    expect(markup).toContain('data-op="removed"');
    expect(markup).toContain('data-op="added"');
    // the removed line keeps its old number, the added line its new one
    expect(markup).toContain('>2</span><span');
    expect(markup).toContain('>new line<');
  });

  test('a read and a search show one line in plain words', () => {
    expect(html(read)).toContain('Read src/lib.rs');
    expect(html(search)).toContain('Search for &quot;fn main&quot; in src');
  });

  test('a pipeline shows its parts, an unknown part as its raw command', () => {
    const markup = html(mixed);
    expect(markup).toContain('Search for &quot;fn main&quot; in src');
    expect(markup).toContain('$ wc -l');
    expect(markup).toContain('Run cargo test');
    expect(markup).toContain('>|<');
    expect(markup).toContain('>&amp;&amp;<');
  });

  test('a reading recognises something only when a step is not unknown', () => {
    expect(recognisesAny(mixed)).toBe(true);
    expect(recognisesAny(unrecognised)).toBe(false);
    expect(recognisesAny(undefined)).toBe(false);
  });
});

describe('PermissionBody', () => {
  const request: PersistedPermissionRequest = {
    id: 'p1',
    mode: 'unsafe',
    script: "apply_patch <<'P'\n*** Begin Patch\n...\nP",
    reading: edit,
  };

  test('an approval for an edit shows the file and diff', () => {
    const markup = renderToStaticMarkup(<PermissionBody request={request} showScript={false} />);
    expect(markup).toContain('~ src/a.rs');
    expect(markup).toContain('old line');
    expect(markup).not.toContain('*** Begin Patch');
  });

  test('the raw script is one step away', () => {
    const markup = renderToStaticMarkup(<PermissionBody request={request} showScript />);
    expect(markup).toContain('*** Begin Patch');
    expect(markup).not.toContain('~ src/a.rs');
  });

  test('a request nothing recognised shows its script', () => {
    const markup = renderToStaticMarkup(
      <PermissionBody request={{ ...request, script: 'echo hi', reading: unrecognised }} showScript={false} />,
    );
    expect(markup).toContain('echo hi');
  });
});
