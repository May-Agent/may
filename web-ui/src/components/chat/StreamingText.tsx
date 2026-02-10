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

  // Diff: only add new content
  if (text.length > store.fullText.length && text.startsWith(store.fullText)) {
    const newContent = text.slice(store.fullText.length);
    store.tokens.push({ text: newContent, key: store.counter++ });
    store.fullText = text;
  } else if (!text.startsWith(store.fullText)) {
    // Text was completely replaced (e.g. markdown re-parse changed structure)
    store.tokens = [{ text, key: store.counter++ }];
    store.fullText = text;
  }

  return (
    <>
      {store.tokens.map((t) => (
        <span
          key={t.key}
          style={{
            animationName: 'streaming-fade',
            animationDuration: '0.35s',
            animationTimingFunction: 'ease-out',
            animationFillMode: 'both',
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
