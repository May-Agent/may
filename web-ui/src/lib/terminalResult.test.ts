import { describe, expect, test } from 'bun:test';
import { extractTerminalResult } from './terminalResult';

describe('extractTerminalResult', () => {
  test('reads inline stdout and the exit code of a command result', () => {
    const terminal = extractTerminalResult({
      kind: 'command',
      stdout: { kind: 'inline', text: 'hello\n' },
      stderr: { kind: 'inline', text: 'warn' },
      outcome: { status: 'exited', code: 2 },
    });
    expect(terminal?.exitCode).toBe(2);
    expect(terminal?.stdout?.text).toBe('hello\n');
    expect(terminal?.stderr?.text).toBe('warn');
  });

  test('stored output shows its preview and links the stored file', () => {
    const terminal = extractTerminalResult({
      kind: 'command',
      stdout: {
        kind: 'stored',
        file: { url: 'outputs/log.txt', path: '/s/outputs/log.txt' },
        preview: 'head',
        truncated: true,
      },
      outcome: { status: 'exited', code: 0 },
    });
    expect(terminal?.stdout).toEqual({ text: 'head', truncated: true, url: 'outputs/log.txt' });
    expect(terminal?.stderr).toBeNull();
  });

  test('image output is labelled and linked', () => {
    const terminal = extractTerminalResult({
      kind: 'command',
      stdout: {
        kind: 'image',
        media_type: 'image/png',
        file: { url: 'outputs/a.png', path: '/s/outputs/a.png' },
      },
      outcome: { status: 'exited', code: 0 },
    });
    expect(terminal?.stdout).toEqual({
      text: '[image: image/png]',
      truncated: false,
      url: 'outputs/a.png',
    });
  });

  test('a run that did not exit has no exit code', () => {
    const terminal = extractTerminalResult({
      kind: 'command',
      stdout: { kind: 'inline', text: '' },
      outcome: { status: 'killed', task_id: 't', notice: 'stopped' },
    });
    expect(terminal?.exitCode).toBeNull();
    expect(terminal?.stdout).toBeNull();
  });

  test('other result kinds are not terminal results', () => {
    expect(extractTerminalResult({ kind: 'text', text: 'x' })).toBeNull();
    expect(extractTerminalResult(null)).toBeNull();
  });
});
