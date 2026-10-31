/**
 * App-wide keyboard shortcuts, following the macOS conventions and their
 * usual Ctrl equivalents elsewhere:
 *
 * - New chat: Cmd/Ctrl+N (browsers may keep it for a new window, so
 *   Cmd/Ctrl+Shift+O also works)
 * - Search and commands: Cmd/Ctrl+K
 * - Show/hide sidebar: Ctrl+Cmd+S on a Mac, Ctrl+Alt+S elsewhere
 * - Show/hide inspector: Option+Cmd+0 on a Mac, Ctrl+Alt+0 elsewhere
 */

export type ShortcutId = 'newChat' | 'search' | 'toggleSidebar' | 'toggleInspector';

interface KeyLike {
  key: string;
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

export function isMacPlatform(): boolean {
  if (typeof navigator === 'undefined') return false;
  const platform =
    (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform ??
    navigator.platform ??
    '';
  return /mac|iphone|ipad|ipod/i.test(platform);
}

/** Which shortcut `e` is, if any. */
export function matchShortcut(e: KeyLike, mac: boolean): ShortcutId | null {
  const mod = mac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  const key = e.key.toLowerCase();
  if (mod && !e.altKey && !e.shiftKey && key === 'n') return 'newChat';
  if (mod && !e.altKey && e.shiftKey && e.code === 'KeyO') return 'newChat';
  if (mod && !e.altKey && !e.shiftKey && key === 'k') return 'search';
  const sidebarChord = mac ? e.metaKey && e.ctrlKey && !e.altKey : e.ctrlKey && e.altKey && !e.metaKey;
  if (sidebarChord && !e.shiftKey && e.code === 'KeyS') return 'toggleSidebar';
  const inspectorChord = mac ? e.metaKey && e.altKey && !e.ctrlKey : e.ctrlKey && e.altKey && !e.metaKey;
  if (inspectorChord && !e.shiftKey && e.code === 'Digit0') return 'toggleInspector';
  return null;
}

const LABELS: Record<ShortcutId, { mac: string; other: string; aria: string; ariaMac: string }> = {
  newChat: { mac: '⌘N', other: 'Ctrl+N', aria: 'Control+N', ariaMac: 'Meta+N' },
  search: { mac: '⌘K', other: 'Ctrl+K', aria: 'Control+K', ariaMac: 'Meta+K' },
  toggleSidebar: { mac: '⌃⌘S', other: 'Ctrl+Alt+S', aria: 'Control+Alt+S', ariaMac: 'Control+Meta+S' },
  toggleInspector: { mac: '⌥⌘0', other: 'Ctrl+Alt+0', aria: 'Control+Alt+0', ariaMac: 'Alt+Meta+0' },
};

/** Glyphs shown next to a command, e.g. `⌘N` or `Ctrl+N`. */
export function shortcutLabel(id: ShortcutId, mac = isMacPlatform()): string {
  return mac ? LABELS[id].mac : LABELS[id].other;
}

/** Value for `aria-keyshortcuts`. */
export function shortcutAria(id: ShortcutId, mac = isMacPlatform()): string {
  return mac ? LABELS[id].ariaMac : LABELS[id].aria;
}
