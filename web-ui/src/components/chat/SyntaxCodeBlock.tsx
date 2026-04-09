import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { oneLight, vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { useThemeMode } from '@/lib/themeMode';

export default function SyntaxCodeBlock({
  language,
  code,
}: {
  language: string;
  code: string;
}) {
  const themeMode = useThemeMode();

  return (
    <SyntaxHighlighter
      style={themeMode === 'dark' ? vscDarkPlus : oneLight}
      language={language}
      PreTag="div"
      customStyle={{
        margin: 0,
        borderRadius: 0,
        fontSize: '0.8125rem',
        background: 'transparent',
      }}
    >
      {code}
    </SyntaxHighlighter>
  );
}
