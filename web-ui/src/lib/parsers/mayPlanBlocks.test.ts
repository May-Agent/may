import { describe, test, expect } from 'bun:test';
import {
  containsMayPlan,
  extractMayPlanBlocks,
  type MayPlanSegment,
} from './mayPlanBlocks';

function onlyPlans(text: string): MayPlanSegment[] {
  return extractMayPlanBlocks(text).filter(
    (segment): segment is MayPlanSegment => segment.kind === 'plan',
  );
}

describe('extractMayPlanBlocks', () => {
  test('returns a single text segment when no plan tags exist', () => {
    const input = 'Hello, world!';
    const segments = extractMayPlanBlocks(input);
    expect(segments).toHaveLength(1);
    expect(segments[0]).toEqual({
      kind: 'text',
      content: input,
      start: 0,
      end: input.length,
    });
  });

  test('extracts a single closed plan block', () => {
    const input =
      'Prelude.\n<may-plan>\n## Title\n- [ ] Step 1\n</may-plan>\nPost.';
    const segments = extractMayPlanBlocks(input);
    expect(segments).toHaveLength(3);
    expect(segments[0].kind).toBe('text');
    expect(segments[0].content).toBe('Prelude.\n');
    expect(segments[1].kind).toBe('plan');
    const plan = segments[1] as MayPlanSegment;
    expect(plan.open).toBe(false);
    expect(plan.status).toBe('draft');
    expect(plan.content).toBe('\n## Title\n- [ ] Step 1\n');
    expect(segments[2].kind).toBe('text');
    expect(segments[2].content).toBe('\nPost.');
  });

  test('parses an explicit status attribute', () => {
    const input =
      '<may-plan status="progress">\n- [x] Done\n</may-plan>';
    const plans = onlyPlans(input);
    expect(plans).toHaveLength(1);
    expect(plans[0].status).toBe('progress');
    expect(plans[0].open).toBe(false);
  });

  test('falls back to draft for an unknown status attribute', () => {
    const input = '<may-plan status="banana">x</may-plan>';
    const plans = onlyPlans(input);
    expect(plans[0].status).toBe('draft');
  });

  test('marks an unclosed plan block as open (streaming)', () => {
    const input = 'Intro.\n<may-plan>\n## Drafting...\n- [ ] Step ';
    const segments = extractMayPlanBlocks(input);
    expect(segments).toHaveLength(2);
    expect(segments[0].kind).toBe('text');
    expect(segments[1].kind).toBe('plan');
    const plan = segments[1] as MayPlanSegment;
    expect(plan.open).toBe(true);
    expect(plan.content.startsWith('\n## Drafting')).toBe(true);
  });

  test('marks a plan open when the tag itself is incomplete', () => {
    const input = 'before <may-plan status="pro';
    const segments = extractMayPlanBlocks(input);
    expect(segments).toHaveLength(2);
    const plan = segments[1] as MayPlanSegment;
    expect(plan.open).toBe(true);
    expect(plan.content).toBe('');
  });

  test('ignores </may-plan> that sits inside a fenced code block', () => {
    const input = [
      '<may-plan>',
      '## Real plan',
      '',
      'Example input:',
      '```xml',
      '</may-plan>',
      '```',
      '',
      '- [ ] still inside real plan',
      '</may-plan>',
      '',
      'After.',
    ].join('\n');
    const plans = onlyPlans(input);
    expect(plans).toHaveLength(1);
    expect(plans[0].open).toBe(false);
    expect(plans[0].content).toContain('still inside real plan');
    expect(plans[0].content).toContain('```xml');
  });

  test('ignores <may-plan> that sits inside a fenced code block', () => {
    const input = [
      'Here is how to emit one:',
      '```markdown',
      '<may-plan>',
      '- demo',
      '</may-plan>',
      '```',
      'End.',
    ].join('\n');
    const segments = extractMayPlanBlocks(input);
    const plans = segments.filter((s) => s.kind === 'plan');
    expect(plans).toHaveLength(0);
    expect(segments).toHaveLength(1);
    expect(segments[0].kind).toBe('text');
  });

  test('handles multiple sequential plan blocks', () => {
    const input =
      '<may-plan>first</may-plan><may-plan status="progress">second</may-plan>';
    const plans = onlyPlans(input);
    expect(plans).toHaveLength(2);
    expect(plans[0].content).toBe('first');
    expect(plans[0].status).toBe('draft');
    expect(plans[1].content).toBe('second');
    expect(plans[1].status).toBe('progress');
  });

  test('does not match <may-planet> or similar look-alikes', () => {
    const input = '<may-planet>not a plan</may-planet>';
    const plans = onlyPlans(input);
    expect(plans).toHaveLength(0);
  });
});

describe('containsMayPlan', () => {
  test('returns true for a closed plan', () => {
    expect(containsMayPlan('pre <may-plan>body</may-plan> post')).toBe(true);
  });

  test('returns true for an unclosed streaming plan', () => {
    expect(containsMayPlan('<may-plan>partial ')).toBe(true);
  });

  test('returns false when no plan is present', () => {
    expect(containsMayPlan('just regular text')).toBe(false);
  });

  test('returns false when the only tag sits inside a code fence', () => {
    const input = '```\n<may-plan>fake</may-plan>\n```';
    expect(containsMayPlan(input)).toBe(false);
  });
});
