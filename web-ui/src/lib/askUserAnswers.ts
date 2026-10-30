import type { PersistedAskUserRequest } from '@/lib/types';
import type { AskUserAnswerEntry } from '@/lib/api/interactions';

/** What the user chose or typed for one section of a question. */
export interface SectionAnswer {
  selection: ReadonlySet<string>;
  custom: string;
}

export const emptySectionAnswer = (): SectionAnswer => ({ selection: new Set(), custom: '' });

export function isAnswered(section: SectionAnswer): boolean {
  return section.custom.trim().length > 0 || section.selection.size > 0;
}

/**
 * The answers to submit, or `null` while any section has no choice and no text:
 * a question is answered, withdrawn or open, so an empty form submits nothing.
 */
export function buildAnswers(
  request: PersistedAskUserRequest,
  sections: ReadonlyMap<string, SectionAnswer>,
): AskUserAnswerEntry[] | null {
  const answers: AskUserAnswerEntry[] = [];
  for (const q of request.questions) {
    const section = sections.get(q.section) ?? emptySectionAnswer();
    if (section.custom.trim().length > 0) {
      answers.push({ section: q.section, answer: { kind: 'custom', text: section.custom.trim() } });
    } else if (section.selection.size > 0) {
      answers.push({
        section: q.section,
        answer: { kind: 'selected', values: Array.from(section.selection) },
      });
    } else {
      return null;
    }
  }
  return answers;
}
