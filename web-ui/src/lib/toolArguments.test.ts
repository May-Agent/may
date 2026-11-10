import { describe, expect, test } from 'bun:test';
import { invalidToolArguments } from './toolArguments';

describe('invalidToolArguments', () => {
  test('a terminal call missing script is invalid', () => {
    expect(invalidToolArguments('terminal', '{"command":"ls"}')).toBe(
      'missing string field `script`',
    );
  });

  test('a terminal call whose arguments are not JSON is invalid and does not throw', () => {
    expect(invalidToolArguments('terminal', 'not json')).not.toBeNull();
    expect(invalidToolArguments('terminal', '')).not.toBeNull();
    expect(invalidToolArguments('terminal', '[1]')).toBe('arguments are not a JSON object');
    expect(invalidToolArguments('terminal', 'null')).toBe('arguments are not a JSON object');
  });

  test('the other terminal tools require their fields', () => {
    expect(invalidToolArguments('terminal_kill', '{}')).toBe('missing string field `task_id`');
    expect(invalidToolArguments('terminal_read', '{"task_id":1}')).toBe(
      'missing string field `task_id`',
    );
    expect(invalidToolArguments('terminal_input', '{"task_id":"a"}')).toBe(
      'missing string field `input`',
    );
  });

  test('well-formed arguments and other tools are valid', () => {
    expect(invalidToolArguments('terminal', '{"script":"ls"}')).toBeNull();
    expect(invalidToolArguments('terminal_input', '{"task_id":"a","input":"x"}')).toBeNull();
    expect(invalidToolArguments('browser', 'garbage')).toBeNull();
  });
});
