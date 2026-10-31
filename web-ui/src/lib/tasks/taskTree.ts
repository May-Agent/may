/**
 * The task tree of a session's `tasks.md`, for the inspector.
 *
 * Only structure reaches the UI: checkbox items with their nesting, the
 * headings that group them, and the issues they link. The model's free-text
 * notes (paragraphs, plain bullets, code blocks) are dropped. The rules
 * follow the daemon's `src/todo_parser.rs` (CommonMark task lists via
 * pulldown-cmark): `- [ ]` / `* [x]` / `+ [X]` / `1. [ ]` items, task
 * markup inside fenced code is ignored, soft-wrapped continuation lines join
 * the item's text with a space, inline code keeps its text.
 *
 * Issue links follow the conventions the agent writes: `owner/repo#123`,
 * a bare `#123`, or a GitHub issue / pull request URL. The `## Roots`
 * section (read by `src/agent_link.rs`) lists the session's root issues.
 * `BLOCKING <request_id>` marks an item waiting on an `ask_user` answer.
 */

export interface IssueRef {
  /** `owner/repo` when the reference names one. */
  repo: string | null;
  number: number;
  /** Explicit URL from the document, when one was written. */
  url: string | null;
  /** Text shown for the reference, e.g. `May-Agent/may#12` or `#12`. */
  label: string;
}

export interface TaskNode {
  text: string;
  checked: boolean;
  issues: IssueRef[];
  /** `ask_user` request id this item waits on, from `BLOCKING <id>`. */
  blockedBy: string | null;
  children: TaskNode[];
}

export interface TaskGroup {
  /** Nearest heading above the group's items; null before any heading. */
  heading: string | null;
  tasks: TaskNode[];
}

export interface TaskTree {
  groups: TaskGroup[];
  /** Issues listed under `## Roots`. */
  roots: IssueRef[];
  total: number;
  completed: number;
}

const ROOTS_HEADING = 'roots';

const FENCE = /^ {0,3}(`{3,}|~{3,})/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const LIST_ITEM = /^(\s*)([-*+]|\d{1,9}[.)])(\s+)(.*)$/;
const TASK_MARKER = /^\[([ xX])\](?:\s+|$)(.*)$/;
const BLOCKING = /\bBLOCKING\s+([A-Za-z0-9_-]+)\s*/;

const ISSUE_URL = /https?:\/\/github\.com\/([\w.-]+\/[\w.-]+)\/(?:issues|pull)\/(\d+)\S*/g;
const ISSUE_REF = /(^|[^\w/&])((?:[\w.-]+\/[\w.-]+)?)#(\d+)\b/g;

function columns(indent: string): number {
  let col = 0;
  for (const ch of indent) col = ch === '\t' ? col + 4 - (col % 4) : col + 1;
  return col;
}

/** Inline markdown reduced to its text: links keep their label, code its content. */
function plainText(markdown: string): string {
  return markdown
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]*)\]\(([^)]*)\)/g, (_m, label: string, href: string) =>
      // Keep the URL of a link whose label is not itself the reference so
      // issue URLs behind a label are still found.
      label.trim().length > 0 ? `${label} <${href}>` : href,
    )
    .replace(/`+([^`]*)`+/g, '$1')
    .replace(/(\*\*|__)(.+?)\1/g, '$2')
    .replace(/(^|\W)[*_](\S(?:.*?\S)?)[*_](?=\W|$)/g, '$1$2')
    .replace(/~~(.+?)~~/g, '$1')
    .replace(/\s+/g, ' ')
    .trim();
}

/** Issue references in `text`, deduplicated, in order of appearance. */
export function findIssueRefs(text: string): IssueRef[] {
  const found: Array<{ at: number; ref: IssueRef }> = [];
  const covered: Array<[number, number]> = [];
  for (const m of text.matchAll(ISSUE_URL)) {
    const at = m.index ?? 0;
    covered.push([at, at + m[0].length]);
    found.push({
      at,
      ref: { repo: m[1], number: Number(m[2]), url: m[0].replace(/[>.,;)]+$/, ''), label: `${m[1]}#${m[2]}` },
    });
  }
  for (const m of text.matchAll(ISSUE_REF)) {
    const at = (m.index ?? 0) + m[1].length;
    if (covered.some(([from, to]) => at >= from && at < to)) continue;
    const repo = m[2] ? m[2] : null;
    found.push({
      at,
      ref: { repo, number: Number(m[3]), url: null, label: `${repo ?? ''}#${m[3]}` },
    });
  }
  found.sort((a, b) => a.at - b.at);
  const seen = new Set<string>();
  const refs: IssueRef[] = [];
  for (const { ref } of found) {
    const key = `${ref.repo ?? ''}#${ref.number}`;
    if (seen.has(key)) continue;
    seen.add(key);
    refs.push(ref);
  }
  return refs;
}

/** Text of an item without the markers the inspector shows separately. */
function displayText(raw: string, refs: IssueRef[]): string {
  let text = raw.replace(BLOCKING, '');
  for (const ref of refs) {
    if (ref.url) text = text.split(` <${ref.url}>`).join('').split(ref.url).join('');
  }
  // A link label kept for the URL scan drops its URL again.
  text = text.replace(/ <https?:\/\/[^>]*>/g, '');
  // References alone in parentheses, e.g. "(#231)", leave no empty brackets.
  for (const ref of refs) {
    text = text.split(`(${ref.label})`).join('');
  }
  text = text.replace(/\(\s*\)/g, '').replace(/\s+/g, ' ').trim();
  // A reference trailing the sentence ("Fix the crash — #12") is shown as a
  // chip only.
  for (;;) {
    const trailing = refs.find((ref) => text.endsWith(ref.label));
    if (!trailing) break;
    const rest = text.slice(0, text.length - trailing.label.length).replace(/[\s:,–—-]+$/, '');
    if (rest.length === 0) break;
    text = rest;
  }
  return text;
}

interface OpenItem {
  /** Column where the item's content starts; deeper lines belong to it. */
  contentColumn: number;
  /** Column of the item's marker. */
  markerColumn: number;
  task: (TaskNode & { raw: string }) | null;
}

/** Parse `tasks.md` into its task tree, leaving out free-text notes. */
export function parseTaskTree(markdown: string): TaskTree {
  const groups: TaskGroup[] = [];
  const roots: IssueRef[] = [];
  const all: Array<TaskNode & { raw: string }> = [];
  let group: TaskGroup | null = null;
  let inRoots = false;
  let fence: string | null = null;
  const open: OpenItem[] = [];
  /** Item whose paragraph a non-blank, non-item line continues. */
  let paragraphOf: OpenItem | null = null;

  const currentGroup = (): TaskGroup => {
    if (!group) {
      group = { heading: null, tasks: [] };
      groups.push(group);
    }
    return group;
  };

  for (const line of markdown.split(/\r?\n/)) {
    const fenceMatch = FENCE.exec(line);
    if (fence) {
      if (fenceMatch && fenceMatch[1][0] === fence[0] && fenceMatch[1].length >= fence.length) {
        fence = null;
      }
      continue;
    }
    if (fenceMatch) {
      fence = fenceMatch[1];
      paragraphOf = null;
      continue;
    }

    if (line.trim().length === 0) {
      paragraphOf = null;
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      open.length = 0;
      paragraphOf = null;
      const title = plainText(heading[2]);
      inRoots = heading[1].length === 2 && title.toLowerCase() === ROOTS_HEADING;
      group = inRoots ? null : { heading: title, tasks: [] };
      if (group) groups.push(group);
      continue;
    }

    const item = LIST_ITEM.exec(line);
    if (item) {
      const markerColumn = columns(item[1]);
      const contentColumn = markerColumn + item[2].length + Math.min(columns(item[3]), 4);
      while (open.length > 0 && open[open.length - 1].contentColumn > markerColumn) open.pop();

      if (inRoots) {
        if (markerColumn === 0) roots.push(...findIssueRefs(plainText(item[4])));
        paragraphOf = null;
        continue;
      }

      const marker = TASK_MARKER.exec(item[4]);
      const entry: OpenItem = { contentColumn, markerColumn, task: null };
      if (marker) {
        const node = {
          raw: marker[2],
          text: '',
          checked: marker[1] !== ' ',
          issues: [],
          blockedBy: null,
          children: [],
        };
        entry.task = node;
        all.push(node);
        const parent = [...open].reverse().find((o) => o.task)?.task ?? null;
        if (parent) parent.children.push(node);
        else currentGroup().tasks.push(node);
      }
      open.push(entry);
      paragraphOf = entry;
      continue;
    }

    // A lazy continuation line of the item's first paragraph.
    if (paragraphOf?.task && open.includes(paragraphOf)) {
      paragraphOf.task.raw = `${paragraphOf.task.raw} ${line.trim()}`;
      continue;
    }
    // Anything else (prose, notes) ends the open lists when it is not indented
    // under them.
    const indent = columns(/^\s*/.exec(line)?.[0] ?? '');
    while (open.length > 0 && open[open.length - 1].contentColumn > indent) open.pop();
    paragraphOf = null;
  }

  let completed = 0;
  for (const node of all) {
    const plain = plainText(node.raw);
    const blocking = BLOCKING.exec(plain);
    node.blockedBy = blocking ? blocking[1] : null;
    node.issues = findIssueRefs(plain);
    node.text = displayText(plain, node.issues);
    if (node.checked) completed += 1;
    delete (node as Partial<typeof node>).raw;
  }

  return {
    groups: groups.filter((g) => g.tasks.length > 0),
    roots,
    total: all.length,
    completed,
  };
}
