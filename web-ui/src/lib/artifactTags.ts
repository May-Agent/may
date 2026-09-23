export interface DeclaredArtifact {
  path: string;
  type: 'slidev';
}

export interface ParsedArtifactText {
  visibleText: string;
  artifacts: DeclaredArtifact[];
}

const ARTIFACT_TAG = /<artifact\b([^<>]*?)\/>/gi;
const URL_ATTRIBUTE = /\burl\s*=\s*(["'])(.*?)\1/i;
const SLIDEV_ATTRIBUTE = /(?:^|\s)slidev(?:\s|$)/i;

function normalizeArtifactPath(path: string): string {
  return path.replace(/^\.?\/?artifacts\//, '');
}

export function parseArtifactTags(text: string): ParsedArtifactText {
  const artifacts: DeclaredArtifact[] = [];
  const visibleText = text.replace(ARTIFACT_TAG, (tag, attributes: string) => {
    const url = URL_ATTRIBUTE.exec(attributes)?.[2]?.trim();
    if (!url || !SLIDEV_ATTRIBUTE.test(attributes)) return tag;
    artifacts.push({ path: normalizeArtifactPath(url), type: 'slidev' });
    return '';
  });
  return { visibleText: visibleText.trim(), artifacts };
}
