import { describe, expect, test } from 'bun:test';
import { parseArtifactTags } from './artifactTags';

describe('parseArtifactTags', () => {
  test('extracts a Slidev control tag from assistant prose', () => {
    expect(parseArtifactTags('Done.\n\n<artifact url="deck/slides.md" slidev />')).toEqual({
      visibleText: 'Done.',
      artifacts: [{ path: 'deck/slides.md', type: 'slidev' }],
    });
  });

  test('normalizes a session-relative artifacts prefix', () => {
    expect(parseArtifactTags('<artifact url="artifacts/deck/slides.md" slidev />')).toEqual({
      visibleText: '',
      artifacts: [{ path: 'deck/slides.md', type: 'slidev' }],
    });
  });

  test('preserves unsupported artifact markup as prose', () => {
    expect(parseArtifactTags('<artifact url="report.md" document />')).toEqual({
      visibleText: '<artifact url="report.md" document />',
      artifacts: [],
    });
  });

  test('does not consume an incomplete streaming tag', () => {
    expect(parseArtifactTags('<artifact url="deck.md" slidev')).toEqual({
      visibleText: '<artifact url="deck.md" slidev',
      artifacts: [],
    });
  });
});
