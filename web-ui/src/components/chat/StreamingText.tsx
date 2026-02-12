import { AnimatedMarkdown } from 'flowtoken';
import 'flowtoken/dist/styles.css';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';

export function FlowMarkdown({
  content,
  isStreaming,
}: {
  content: string;
  isStreaming: boolean;
}) {
  return (
    <AnimatedMarkdown
      content={content}
      sep="word"
      animation={isStreaming ? 'fadeIn' : null}
      animationDuration="0.3s"
      animationTimingFunction="ease-out"
      codeStyle={vscDarkPlus}
    />
  );
}
