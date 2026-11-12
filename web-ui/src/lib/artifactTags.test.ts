import { describe, expect, test } from 'bun:test';
import { parseArtifactTags } from './artifactTags';

describe('parseArtifactTags', () => {
  test('extracts a Slidev control tag from assistant prose', () => {
    expect(parseArtifactTags('Done.\n\n<artifact url="deck/slides.md" slidev />')).toEqual({
      visibleText: 'Done.',
      artifacts: [{ path: 'deck/slides.md', type: 'slidev' }],
    });
  });

  test('extracts a file artifact tag', () => {
    expect(parseArtifactTags('Here you go.\n\n<artifact url="report.md" />')).toEqual({
      visibleText: 'Here you go.',
      artifacts: [{ path: 'report.md', type: 'file' }],
    });
  });

  test('accepts a slidev type attribute', () => {
    expect(parseArtifactTags('<artifact url="deck.md" type="slidev" />')).toEqual({
      visibleText: '',
      artifacts: [{ path: 'deck.md', type: 'slidev' }],
    });
  });

  test('normalizes a session-relative artifacts prefix', () => {
    expect(parseArtifactTags('<artifact url="artifacts/deck/slides.md" slidev />')).toEqual({
      visibleText: '',
      artifacts: [{ path: 'deck/slides.md', type: 'slidev' }],
    });
  });

  test('preserves artifact markup without a url as prose', () => {
    expect(parseArtifactTags('<artifact name="report.md" />')).toEqual({
      visibleText: '<artifact name="report.md" />',
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
