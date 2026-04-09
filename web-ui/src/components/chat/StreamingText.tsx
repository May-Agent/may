import { AnimatedMarkdown } from 'flowtoken';
import 'flowtoken/dist/styles.css';
import { oneLight, vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { useThemeMode } from '@/lib/themeMode';

export function FlowMarkdown({
  content,
  isStreaming,
}: {
  content: string;
  isStreaming: boolean;
}) {
  const themeMode = useThemeMode();

  return (
    <AnimatedMarkdown
      content={content}
      sep="word"
      animation={isStreaming ? 'fadeIn' : null}
      animationDuration="0.3s"
      animationTimingFunction="ease-out"
      codeStyle={themeMode === 'dark' ? vscDarkPlus : oneLight}
    />
  );
}
