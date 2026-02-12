import React from 'react';

interface TokenEntry {
  text: string;
  key: number;
}

interface TokenStore {
  tokens: TokenEntry[];
  fullText: string;
  counter: number;
}

const stores = new Map<string, TokenStore>();

function getOrCreate(id: string): TokenStore {
  if (!stores.has(id)) {
    stores.set(id, { tokens: [], fullText: '', counter: 0 });
  }
  return stores.get(id)!;
}

export function clearTokenStore(id: string) {
  stores.delete(id);
}

export function TokenizedStream({ storeId, children }: { storeId: string; children: React.ReactNode }) {
  const text = extractText(children);
  const store = getOrCreate(storeId);

  const pushTokens = (segment: string) => {
    if (!segment) return;
    const pieces = segment.match(/\s+|[^\s]+/g) ?? [segment];
    for (const piece of pieces) {
      store.tokens.push({ text: piece, key: store.counter++ });
    }
  };

  // Diff: only add new content
  if (text.length > store.fullText.length && text.startsWith(store.fullText)) {
    const newContent = text.slice(store.fullText.length);
    pushTokens(newContent);
    store.fullText = text;
  } else if (!text.startsWith(store.fullText)) {
    // Text was completely replaced (e.g. markdown re-parse changed structure)
    store.tokens = [];
    pushTokens(text);
    store.fullText = text;
  }

  return (
    <>
      {store.tokens.map((t) => (
        <span
          key={t.key}
          style={{
            animationName: 'streaming-token-fade',
            animationDuration: '220ms',
            animationTimingFunction: 'ease-out',
            animationFillMode: 'both',
            display: 'inline',
          }}
        >
          {t.text}
        </span>
      ))}
    </>
  );
}

function extractText(node: React.ReactNode): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (!node) return '';
  if (Array.isArray(node)) return node.map(extractText).join('');
  if (typeof node === 'object' && 'props' in node) {
    return extractText((node as any).props.children);
  }
  return '';
}
