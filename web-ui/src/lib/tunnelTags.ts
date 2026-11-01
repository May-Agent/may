export interface ParsedTunnelText {
  visibleText: string;
  tunnels: string[];
}

const TUNNEL_TAG = /<tunnel\b([^<>]*?)\/>/gi;
const ID_ATTRIBUTE = /\bid\s*=\s*(["'])(.*?)\1/i;

export function parseTunnelTags(text: string): ParsedTunnelText {
  const tunnels: string[] = [];
  const visibleText = text.replace(TUNNEL_TAG, (tag, attributes: string) => {
    const id = ID_ATTRIBUTE.exec(attributes)?.[2]?.trim();
    if (!id) return tag;
    tunnels.push(id);
    return '';
  });
  return { visibleText: visibleText.trim(), tunnels };
}
