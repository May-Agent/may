/**
 * Views over the rows of a session's `tasks.md`.
 *
 * The daemon parses the document (aither's `TasksFile`) and sends the rows;
 * nothing here reads markdown.
 */

import type { TaskReference, TaskRow, TaskSection } from '../types';

/** Sections that hold open work, in display order. */
export const OPEN_SECTIONS: readonly TaskSection[] = ['doing', 'queued'];

export interface OpenGroup {
  section: TaskSection;
  rows: TaskRow[];
}

/** Rows of `Doing` and `Queued` grouped by section; empty groups are left out. */
export function openGroups(rows: readonly TaskRow[]): OpenGroup[] {
  return OPEN_SECTIONS.map((section) => ({
    section,
    rows: rows.filter((row) => row.section === section),
  })).filter((group) => group.rows.length > 0);
}

/** References named by the `Roots` rows, without repeats. */
export function rootReferences(rows: readonly TaskRow[]): TaskReference[] {
  const seen = new Set<string>();
  const out: TaskReference[] = [];
  for (const row of rows) {
    if (row.section !== 'roots') continue;
    for (const reference of row.references) {
      const label = referenceLabel(reference);
      if (seen.has(label)) continue;
      seen.add(label);
      out.push(reference);
    }
  }
  return out;
}

/** The reference as `tasks.md` writes it: `#12`, `owner/repo#12`, `PR #12`. */
export function referenceLabel(reference: TaskReference): string {
  const text = `${reference.repository ?? ''}#${reference.number}`;
  return reference.pullRequest ? `PR ${text}` : text;
}

/** The rows whose `BLOCKING` mark names the `ask_user` question `requestId`. */
export function rowsBlockedBy(rows: readonly TaskRow[], requestId: string): TaskRow[] {
  return rows.filter((row) => row.blocking === requestId);
}

/** Number of rows that are open work. */
export function openCount(rows: readonly TaskRow[]): number {
  return rows.filter((row) => OPEN_SECTIONS.includes(row.section)).length;
}
