import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';

export default function SyntaxCodeBlock({
  language,
  code,
}: {
  language: string;
  code: string;
}) {
  return (
    <SyntaxHighlighter
      style={vscDarkPlus}
      language={language}
      PreTag="div"
      customStyle={{ margin: 0, borderRadius: 0, fontSize: '0.8125rem' }}
    >
      {code}
    </SyntaxHighlighter>
  );
}
