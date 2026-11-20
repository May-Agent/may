import { PrismLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import bash from 'react-syntax-highlighter/dist/esm/languages/prism/bash';
import python from 'react-syntax-highlighter/dist/esm/languages/prism/python';
import oneLight from 'react-syntax-highlighter/dist/esm/styles/prism/one-light';
import type { PersistedPermissionRequest } from '../../lib/types';
import { recognisesAny } from '../../lib/commandReading';
import { CommandReadingView } from '../chat/CommandReadingView';
import { scriptLanguage } from '../chat/ToolCard';

SyntaxHighlighter.registerLanguage('bash', bash);
SyntaxHighlighter.registerLanguage('python', python);

interface PermissionBodyProps {
  request: PersistedPermissionRequest;
  /** Show the raw script instead of the reading. */
  showScript: boolean;
}

/**
 * What an approval prompt shows of the request: how the daemon read the
 * script (the file and diff for an edit), or the raw script when the
 * reading recognised nothing or the user asked for it.
 */
export function PermissionBody({ request, showScript }: PermissionBodyProps) {
  const reading = request.reading;
  if (recognisesAny(reading) && !showScript) {
    return (
      <div className="rounded-md border border-border bg-muted/40 px-3 py-2">
        <CommandReadingView reading={reading} />
      </div>
    );
  }
  return (
    <div className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono max-h-64 overflow-auto scrollbar-thin">
      <SyntaxHighlighter
        language={scriptLanguage(request.interpreter)}
        style={oneLight}
        customStyle={{ background: 'transparent', padding: 0, margin: 0 }}
        codeTagProps={{ style: { background: 'transparent', fontFamily: 'inherit' } }}
        PreTag="div"
      >
        {request.script}
      </SyntaxHighlighter>
    </div>
  );
}
