import { describe, expect, test } from 'bun:test';
import {
  parseArtifactValidationMessage,
  type ArtifactValidationMessage,
} from './artifactValidation';

describe('parseArtifactValidationMessage', () => {
  test('accepts diagnostics for the expected artifact type', () => {
    const message: ArtifactValidationMessage = {
      source: 'may-artifact-runtime',
      version: 1,
      artifactType: 'html',
      diagnostics: ['console.error: render failed'],
    };
    expect(parseArtifactValidationMessage(message, 'html')).toEqual(message);
  });

  test('rejects messages from another artifact type', () => {
    expect(
      parseArtifactValidationMessage(
        {
          source: 'may-artifact-runtime',
          version: 1,
          artifactType: 'html',
          diagnostics: [],
        },
        'slidev',
      ),
    ).toBeNull();
  });
});
