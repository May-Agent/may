export type DeclaredArtifactType = 'slidev' | 'file';

export interface DeclaredArtifact {
  path: string;
  type: DeclaredArtifactType;
}

export interface ParsedArtifactText {
  visibleText: string;
  artifacts: DeclaredArtifact[];
}

const ARTIFACT_TAG = /<artifact\b([^<>]*?)\/>/gi;
const URL_ATTRIBUTE = /\burl\s*=\s*(["'])(.*?)\1/i;
const TYPE_ATTRIBUTE = /\btype\s*=\s*(["'])(.*?)\1/i;
const SLIDEV_ATTRIBUTE = /(?:^|\s)slidev(?:\s|$)/i;

function normalizeArtifactPath(path: string): string {
  return path.replace(/^\.?\/?artifacts\//, '');
}

export function parseArtifactTags(text: string): ParsedArtifactText {
  const artifacts: DeclaredArtifact[] = [];
  const visibleText = text.replace(ARTIFACT_TAG, (tag, attributes: string) => {
    const url = URL_ATTRIBUTE.exec(attributes)?.[2]?.trim();
    if (!url) return tag;
    const typeAttr = TYPE_ATTRIBUTE.exec(attributes)?.[2]?.trim().toLowerCase();
    const type: DeclaredArtifactType =
      typeAttr === 'slidev' || (!typeAttr && SLIDEV_ATTRIBUTE.test(attributes))
        ? 'slidev'
        : 'file';
    artifacts.push({ path: normalizeArtifactPath(url), type });
    return '';
  });
  return { visibleText: visibleText.trim(), artifacts };
}
