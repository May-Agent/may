import { describe, expect, test } from 'bun:test';
import { findIssueRefs, parseTaskTree, type TaskNode } from './taskTree';

/** The tree reduced to `[x] text` lines indented by depth, for compact asserts. */
function outline(nodes: TaskNode[], depth = 0): string[] {
  return nodes.flatMap((node) => [
    `${'  '.repeat(depth)}[${node.checked ? 'x' : ' '}] ${node.text}`,
    ...outline(node.children, depth + 1),
  ]);
}

describe('parseTaskTree', () => {
  test('keeps checkbox items with their nesting and drops free-text notes', () => {
    const tree = parseTaskTree(`# Plan

Notes: the parser flattens nesting today. I think we should keep the
flat API for the TUI.

- a plain bullet the model wrote as a note
- [x] Top level
  - [ ] Child
    - [x] Grandchild

  Some prose under the item, still a note.
- [ ] Second

Closing thoughts that are not tasks.
`);
    expect(tree.groups).toHaveLength(1);
    expect(tree.groups[0].heading).toBe('Plan');
    expect(outline(tree.groups[0].tasks)).toEqual([
      '[x] Top level',
      '  [ ] Child',
      '    [x] Grandchild',
      '[ ] Second',
    ]);
    expect(tree.total).toBe(4);
    expect(tree.completed).toBe(2);
    const texts = JSON.stringify(tree);
    expect(texts).not.toContain('Notes');
    expect(texts).not.toContain('plain bullet');
    expect(texts).not.toContain('prose');
    expect(texts).not.toContain('Closing');
  });

  test('groups items under their headings and skips headings without tasks', () => {
    const tree = parseTaskTree(`# Plan
Intro.
## Phase 1
- [x] Step one
* [ ] Step two
## Decisions
We chose SQLite.
## Phase 2
+ [X] Step three
1. [ ] Step four
`);
    expect(tree.groups.map((g) => g.heading)).toEqual(['Phase 1', 'Phase 2']);
    expect(outline(tree.groups[1].tasks)).toEqual(['[x] Step three', '[ ] Step four']);
  });

  test('ignores task markup inside fenced code blocks', () => {
    const tree = parseTaskTree(`- [x] Real item

\`\`\`md
- [ ] Not a real item
\`\`\`

~~~
- [x] Also not real
~~~

- [ ] Another real item`);
    expect(outline(tree.groups[0].tasks)).toEqual(['[x] Real item', '[ ] Another real item']);
  });

  test('joins soft-wrapped lines and strips inline markdown', () => {
    const tree = parseTaskTree(`- [ ] Run \`cargo test\` and **verify**
  that everything passes
- [ ] Read [the docs](https://example.com/docs)`);
    expect(outline(tree.groups[0].tasks)).toEqual([
      '[ ] Run cargo test and verify that everything passes',
      '[ ] Read the docs',
    ]);
  });

  test('a task nested under a plain bullet attaches to the nearest task', () => {
    const tree = parseTaskTree(`- [ ] Parent
  - context bullet
    - [ ] Child under a note`);
    expect(outline(tree.groups[0].tasks)).toEqual(['[ ] Parent', '  [ ] Child under a note']);
  });

  test('extracts linked issues and BLOCKING markers', () => {
    const tree = parseTaskTree(`## Roots
- May-Agent/may#231
- https://github.com/May-Agent/may/issues/236

## Work
- [x] List edge cases (#231)
- [ ] Nested children keep their parent — May-Agent/may#236
- [ ] Fix it, see https://github.com/o/r/pull/7.
- [ ] BLOCKING req-42 Decide whether issue refs render as links
- [ ] Mention C#1 and &#35; without linking`);
    expect(tree.roots.map((r) => r.label)).toEqual(['May-Agent/may#231', 'May-Agent/may#236']);
    expect(tree.roots[1].url).toBe('https://github.com/May-Agent/may/issues/236');
    // The Roots section is not a task group.
    expect(tree.groups.map((g) => g.heading)).toEqual(['Work']);

    const [edge, nested, fix, blocked, noLinks] = tree.groups[0].tasks;
    expect(edge.text).toBe('List edge cases');
    expect(edge.issues.map((r) => r.label)).toEqual(['#231']);
    expect(nested.text).toBe('Nested children keep their parent');
    expect(nested.issues).toEqual([
      { repo: 'May-Agent/may', number: 236, url: null, label: 'May-Agent/may#236' },
    ]);
    expect(fix.issues[0]).toEqual({
      repo: 'o/r',
      number: 7,
      url: 'https://github.com/o/r/pull/7',
      label: 'o/r#7',
    });
    expect(fix.text).toBe('Fix it, see .');
    expect(blocked.blockedBy).toBe('req-42');
    expect(blocked.text).toBe('Decide whether issue refs render as links');
    expect(noLinks.issues).toEqual([]);
  });

  test('empty and note-only documents have no tasks', () => {
    expect(parseTaskTree('')).toEqual({ groups: [], roots: [], total: 0, completed: 0 });
    expect(parseTaskTree('# Notes\nJust thinking.\n- a bullet').total).toBe(0);
  });
});

describe('findIssueRefs', () => {
  test('deduplicates a reference written twice', () => {
    expect(findIssueRefs('#3 and #3 and o/r#3').map((r) => r.label)).toEqual(['#3', 'o/r#3']);
  });
});
