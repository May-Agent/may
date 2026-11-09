import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { TaskRow } from '@/lib/types';
import { TaskRowsView } from './TaskRowsView';

const row = (title: string, detail: string, extra: Partial<TaskRow> = {}): TaskRow => ({
  section: 'doing',
  title,
  detail,
  blocking: null,
  references: [],
  ...extra,
});

describe('TaskRowsView', () => {
  test('shows each row title and never its detail', () => {
    const markup = renderToStaticMarkup(
      <TaskRowsView
        rows={[
          row('wire the command', 'the detail stays hidden'),
          row('写文档 ドキュメント 문서', '詳細は表示しない', { section: 'queued' }),
        ]}
      />,
    );
    expect(markup).toContain('wire the command');
    expect(markup).toContain('写文档 ドキュメント 문서');
    expect(markup).not.toContain('the detail stays hidden');
    expect(markup).not.toContain('詳細は表示しない');
  });
});
