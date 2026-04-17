import { Streamdown } from 'streamdown';
import { makeMarkdownComponents } from './shared/MarkdownRenderer';

export function FlowMarkdown({
  content,
  isStreaming,
}: {
  content: string;
  isStreaming: boolean;
}) {
  const components = makeMarkdownComponents(false, isStreaming);

  return (
    <Streamdown
      mode="streaming"
      parseIncompleteMarkdown
      isAnimating={isStreaming}
      remend={{
        bold: true,
        italic: true,
        links: true,
        inlineCode: true,
        strikethrough: true,
        katex: false,
      }}
      linkSafety={{ enabled: false }}
      controls={{ code: false, table: false }}
      components={components}
    >
      {content}
    </Streamdown>
  );
}
