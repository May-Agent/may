import { describe, expect, test } from 'bun:test';
import { matchShortcut, shortcutLabel } from './shortcuts';

const key = (k: string, code: string, mods: Partial<Record<'meta' | 'ctrl' | 'alt' | 'shift', boolean>> = {}) => ({
  key: k,
  code,
  metaKey: mods.meta ?? false,
  ctrlKey: mods.ctrl ?? false,
  altKey: mods.alt ?? false,
  shiftKey: mods.shift ?? false,
});

describe('matchShortcut', () => {
  test('uses Cmd on a Mac and Ctrl elsewhere', () => {
    expect(matchShortcut(key('n', 'KeyN', { meta: true }), true)).toBe('newChat');
    expect(matchShortcut(key('n', 'KeyN', { ctrl: true }), true)).toBeNull();
    expect(matchShortcut(key('n', 'KeyN', { ctrl: true }), false)).toBe('newChat');
    expect(matchShortcut(key('K', 'KeyK', { ctrl: true }), false)).toBe('search');
    expect(matchShortcut(key('O', 'KeyO', { meta: true, shift: true }), true)).toBe('newChat');
  });

  test('sidebar and inspector chords match by physical key', () => {
    expect(matchShortcut(key('s', 'KeyS', { meta: true, ctrl: true }), true)).toBe('toggleSidebar');
    expect(matchShortcut(key('ß', 'KeyS', { ctrl: true, alt: true }), false)).toBe('toggleSidebar');
    // Option+0 types "º" on a Mac keyboard.
    expect(matchShortcut(key('º', 'Digit0', { meta: true, alt: true }), true)).toBe('toggleInspector');
    expect(matchShortcut(key('0', 'Digit0', { meta: true }), true)).toBeNull();
  });

  test('plain typing is never a shortcut', () => {
    expect(matchShortcut(key('n', 'KeyN'), true)).toBeNull();
    expect(matchShortcut(key('Enter', 'Enter', { shift: true }), false)).toBeNull();
  });

  test('labels follow the platform', () => {
    expect(shortcutLabel('newChat', true)).toBe('⌘N');
    expect(shortcutLabel('toggleInspector', false)).toBe('Ctrl+Alt+0');
  });
});
