import { describe, expect, test } from 'bun:test';
import { extractStringField } from './partialJson';

describe('extractStringField', () => {
  test('returns undefined on empty buffer', () => {
    expect(extractStringField('', 'description')).toBeUndefined();
  });

  test('returns undefined when key missing', () => {
    expect(extractStringField('{"script":"ls"}', 'description')).toBeUndefined();
  });

  test('complete string value', () => {
    expect(extractStringField('{"description":"hello"}', 'description')).toBe(
      'hello',
    );
  });

  test('partial string — no closing quote yet', () => {
    expect(extractStringField('{"description":"hel', 'description')).toBe('hel');
  });

  test('empty string value with closing quote', () => {
    expect(extractStringField('{"description":""}', 'description')).toBe('');
  });

  test('returns empty string when key exists but value has not started', () => {
    expect(extractStringField('{"description":', 'description')).toBe('');
  });

  test('escaped quote inside value', () => {
    expect(
      extractStringField('{"description":"he said \\"hi\\""}', 'description'),
    ).toBe('he said "hi"');
  });

  test('escaped backslash', () => {
    expect(extractStringField('{"script":"a\\\\b"}', 'script')).toBe('a\\b');
  });

  test('escape sequences decode to raw control chars', () => {
    expect(extractStringField('{"script":"a\\nb\\tc"}', 'script')).toBe(
      'a\nb\tc',
    );
  });

  test('unicode escape complete', () => {
    expect(extractStringField('{"description":"\\u00e9clair"}', 'description')).toBe(
      'éclair',
    );
  });

  test('unicode escape truncated — returns prefix and stops', () => {
    expect(extractStringField('{"description":"pre \\u00', 'description')).toBe(
      'pre ',
    );
  });

  test('backslash at buffer end is treated as incomplete escape', () => {
    expect(extractStringField('{"description":"abc\\', 'description')).toBe('abc');
  });

  test('forward slash escape', () => {
    expect(extractStringField('{"description":"a\\/b"}', 'description')).toBe(
      'a/b',
    );
  });

  test('field ordering: description then script', () => {
    const buf = '{"description":"say hi","script":"echo hi"}';
    expect(extractStringField(buf, 'description')).toBe('say hi');
    expect(extractStringField(buf, 'script')).toBe('echo hi');
  });

  test('field ordering: script before description', () => {
    const buf = '{"script":"echo hi","description":"say hi"}';
    expect(extractStringField(buf, 'description')).toBe('say hi');
    expect(extractStringField(buf, 'script')).toBe('echo hi');
  });

  test('braces inside string literal do not confuse scan', () => {
    expect(
      extractStringField('{"script":"echo {a: 1}"}', 'script'),
    ).toBe('echo {a: 1}');
  });

  test('whitespace around colon is tolerated', () => {
    expect(
      extractStringField('{"description"  :  "ok"}', 'description'),
    ).toBe('ok');
  });

  test('returns undefined when matched token is a substring of another key', () => {
    expect(
      extractStringField('{"my_description":"fake","description":"real"}', 'description'),
    ).toBe('real');
  });

  test('partial buffer captured mid-first-character of value', () => {
    expect(extractStringField('{"description":"', 'description')).toBe('');
  });

  test('streaming: decoded prefix grows monotonically across buffer extensions', () => {
    const steps = [
      '{"description":"He',
      '{"description":"Hell',
      '{"description":"Hello',
      '{"description":"Hello,',
      '{"description":"Hello, wor',
      '{"description":"Hello, world"}',
    ];
    const decoded = steps.map((s) => extractStringField(s, 'description'));
    expect(decoded).toEqual([
      'He',
      'Hell',
      'Hello',
      'Hello,',
      'Hello, wor',
      'Hello, world',
    ]);
  });

  test('malformed \\uXXX (only 3 hex) emits verbatim and continues', () => {
    // Buffer has \u and only three hex chars before closing quote -> regex
    // requires 4 hex; since buffer length is sufficient (6 bytes after i),
    // the hex validator fails and escape is emitted as "\u" then "a" is read.
    expect(extractStringField('{"description":"\\u00ax"}', 'description')).toBe(
      '\\u00ax',
    );
  });
});

describe('extractStringField — terminal tool streaming', () => {
  // The terminal-tool argument payload the agent emits looks like:
  //   {"description":"…","script":"…"}
  // The SSE `tool_delta` event carries slices of this JSON buffer as the LLM
  // produces it. The web UI accumulates every fragment and calls
  // `extractStringField` against the growing buffer to render `description` and
  // `script` as typewriter text. These tests pin that projection.

  function accumulateByChar(fullPayload: string): {
    description: Array<string | undefined>;
    script: Array<string | undefined>;
  } {
    const description: Array<string | undefined> = [];
    const script: Array<string | undefined> = [];
    let buffer = '';
    for (const ch of fullPayload) {
      buffer += ch;
      description.push(extractStringField(buffer, 'description'));
      script.push(extractStringField(buffer, 'script'));
    }
    return { description, script };
  }

  test('description streams char-by-char ahead of script', () => {
    const payload = '{"description":"run ls","script":"ls -la"}';
    const { description, script } = accumulateByChar(payload);

    // The colon for "description" lands at payload index 14, so after
    // consuming 14 characters the key has been matched but the value hasn't
    // started — the decoder surfaces an empty string. The first real char of
    // the value ("r") is at index 16 in the payload, which is description[16].
    expect(description[12]).toBeUndefined();
    // After the colon but before any value char, extractStringField
    // returns ''. `description[13]` is buffer `{"description"` (no colon yet).
    expect(description[13]).toBeUndefined();
    expect(description[14]).toBe(''); // just consumed the ':'
    expect(description[15]).toBe(''); // just consumed the opening '"'
    expect(description[16]).toBe('r');
    expect(description[17]).toBe('ru');
    expect(description[18]).toBe('run');
    expect(description[19]).toBe('run ');
    expect(description[20]).toBe('run l');
    expect(description[21]).toBe('run ls');

    // description must be stable (never shorter) once complete.
    for (let i = 21; i < description.length; i += 1) {
      expect(description[i]).toBe('run ls');
    }

    // script only starts appearing after its key is observed.
    expect(script[payload.length - 1]).toBe('ls -la');
    const firstScriptSeen = script.findIndex((v) => v !== undefined);
    expect(firstScriptSeen).toBeGreaterThan(0);
  });

  test('realistic multi-word script streams monotonically', () => {
    const payload =
      '{"description":"Search repo for TODOs","script":"rg --line-number TODO src"}';
    const descSnapshots: string[] = [];
    const scriptSnapshots: string[] = [];
    let buffer = '';

    for (const ch of payload) {
      buffer += ch;
      const desc = extractStringField(buffer, 'description');
      const script = extractStringField(buffer, 'script');
      if (desc !== undefined) descSnapshots.push(desc);
      if (script !== undefined) scriptSnapshots.push(script);
    }

    expect(descSnapshots.at(-1)).toBe('Search repo for TODOs');
    expect(scriptSnapshots.at(-1)).toBe('rg --line-number TODO src');

    // Monotonic: each snapshot must be a prefix of the next.
    for (let i = 1; i < descSnapshots.length; i += 1) {
      expect(descSnapshots[i].startsWith(descSnapshots[i - 1])).toBe(true);
    }
    for (let i = 1; i < scriptSnapshots.length; i += 1) {
      expect(scriptSnapshots[i].startsWith(scriptSnapshots[i - 1])).toBe(true);
    }
  });

  test('escape arriving across chunk boundary surfaces consistently', () => {
    // LLM emits the script in three chunks, splitting an escape sequence
    // across the boundary. We want the user to see:
    //   step1: "echo "      (backslash dropped until full escape lands)
    //   step2: "echo \""    (escape complete, closing decoded as `"`)
    //   step3: "echo \"hi\""
    const chunk1 = '{"script":"echo ';
    const chunk2 = '\\';
    const chunk3 = '"hi\\""}';

    const buf1 = chunk1;
    const buf2 = chunk1 + chunk2;
    const buf3 = chunk1 + chunk2 + chunk3;

    expect(extractStringField(buf1, 'script')).toBe('echo ');
    // Pending backslash — decoder must return the prefix without consuming it.
    expect(extractStringField(buf2, 'script')).toBe('echo ');
    expect(extractStringField(buf3, 'script')).toBe('echo "hi"');
  });

  test('arbitrary-width chunking produces monotonic prefixes', () => {
    const payload = '{"description":"hello world","script":"echo hi"}';
    // Simulate variable-width network chunks.
    const chunkSizes = [1, 4, 2, 8, 3, 10, 5, 999];
    let cursor = 0;
    let buffer = '';
    let lastDesc = '';
    let lastScript = '';

    for (const size of chunkSizes) {
      if (cursor >= payload.length) break;
      const take = Math.min(size, payload.length - cursor);
      buffer += payload.slice(cursor, cursor + take);
      cursor += take;

      const desc = extractStringField(buffer, 'description') ?? '';
      const script = extractStringField(buffer, 'script') ?? '';

      expect(desc.startsWith(lastDesc)).toBe(true);
      expect(script.startsWith(lastScript)).toBe(true);

      lastDesc = desc;
      lastScript = script;
    }

    expect(lastDesc).toBe('hello world');
    expect(lastScript).toBe('echo hi');
  });

  test('newline and tab escapes in script decode into visible control chars', () => {
    const payload = '{"script":"echo one\\n'; // closes later
    expect(extractStringField(payload, 'script')).toBe('echo one\n');

    const payload2 = '{"script":"line1\\n\\tindented"}';
    expect(extractStringField(payload2, 'script')).toBe('line1\n\tindented');
  });

  test('unicode escape split across chunks stops at partial sequence', () => {
    const steps = [
      '{"description":"smile ',
      '{"description":"smile \\',
      '{"description":"smile \\u',
      '{"description":"smile \\u26',
      '{"description":"smile \\u263',
      '{"description":"smile \\u263A"}',
    ];
    const decoded = steps.map((s) => extractStringField(s, 'description'));
    expect(decoded[0]).toBe('smile ');
    // Backslash / partial `\u…` — decoder holds back until complete.
    expect(decoded[1]).toBe('smile ');
    expect(decoded[2]).toBe('smile ');
    expect(decoded[3]).toBe('smile ');
    expect(decoded[4]).toBe('smile ');
    // Full \u263A = ☺
    expect(decoded[5]).toBe('smile \u263A');
  });
});
