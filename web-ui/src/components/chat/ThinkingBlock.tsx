import { useState, useEffect, useRef } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';

const MAX_BLUR = 3;
const BLUR_WINDOW = 8;

function StreamingThinkingText({ content }: { content: string }) {
  const [segments, setSegments] = useState<{ id: number; text: string }[]>([]);
  const prevLenRef = useRef(0);
  const idRef = useRef(0);

  useEffect(() => {
    const prevLen = prevLenRef.current;
    if (content.length > prevLen) {
      const newText = content.slice(prevLen);
      const newId = ++idRef.current;
      setSegments(prev => [...prev, { id: newId, text: newText }]);
      prevLenRef.current = content.length;
    }
  }, [content]);

  const total = segments.length;

  return (
    <span>
      {segments.map((seg, i) => {
        const distFromEnd = total - 1 - i;
        if (distFromEnd >= BLUR_WINDOW) {
          return <span key={seg.id}>{seg.text}</span>;
        }
        const blur = MAX_BLUR * (1 - distFromEnd / BLUR_WINDOW);
        return (
          <motion.span
            key={seg.id}
            initial={{ opacity: 0, filter: `blur(${blur}px)` }}
            animate={{ opacity: 1, filter: 'blur(0px)' }}
            transition={{ duration: 0.35, ease: 'easeOut' }}
          >
            {seg.text}
          </motion.span>
        );
      })}
    </span>
  );
}

interface ThinkingBlockProps {
  thinking: string;
  isThinking: boolean;
}

export function ThinkingBlock({ thinking, isThinking }: ThinkingBlockProps) {
  const [isExpanded, setIsExpanded] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const startRef = useRef(Date.now());
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!isThinking) return;
    const interval = setInterval(() => setElapsed(Math.floor((Date.now() - startRef.current) / 1000)), 1000);
    return () => clearInterval(interval);
  }, [isThinking]);

  useEffect(() => {
    if (!isThinking) setElapsed(Math.floor((Date.now() - startRef.current) / 1000));
  }, [isThinking]);

  // Auto-scroll to bottom during thinking when not expanded
  useEffect(() => {
    if (isThinking && !isExpanded && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [thinking, isThinking, isExpanded]);

  const showCompact = isThinking && !isExpanded;

  return (
    <div className="mb-3">
      {/* Header button */}
      <button
        onClick={() => setIsExpanded(!isExpanded)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors mb-1.5"
      >
        <motion.div
          animate={{ rotate: isExpanded ? 90 : 0 }}
          transition={{ duration: 0.15 }}
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </motion.div>
        <span className="font-medium">
          {isThinking ? 'Thinking' : `Thought for ${elapsed}s`}
          {isThinking && (
            <motion.span
              className="inline-block ml-0.5"
              animate={{ opacity: [1, 0.3] }}
              transition={{ duration: 0.8, repeat: Infinity, repeatType: 'reverse' }}
            >...</motion.span>
          )}
        </span>
      </button>

      {/* Compact scrolling box during thinking */}
      {showCompact && (
        <div
          className="relative rounded-lg border border-border/40 bg-muted/30 overflow-hidden cursor-pointer"
          onClick={() => setIsExpanded(true)}
        >
          <div
            ref={scrollRef}
            className="max-h-[120px] overflow-y-auto px-3 py-2 text-sm text-muted-foreground leading-relaxed scrollbar-none"
          >
            <StreamingThinkingText content={thinking} />
          </div>
          {/* Top fade mask */}
          <div className="absolute inset-x-0 top-0 h-6 bg-gradient-to-b from-muted/30 to-transparent pointer-events-none rounded-t-lg" />
          {/* Bottom fade mask */}
          <div className="absolute inset-x-0 bottom-0 h-8 bg-gradient-to-t from-muted/30 to-transparent pointer-events-none rounded-b-lg" />
        </div>
      )}

      {/* Collapsed state after thinking - just the header, no content */}
      {/* (header above already shows "Thought for Xs") */}

      {/* Expanded full content */}
      <AnimatePresence initial={false}>
        {isExpanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: 'auto', opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.25, ease: 'easeInOut' }}
            className="overflow-hidden"
          >
            <div className={cn(
              'rounded-lg border border-border/40 bg-muted/30 px-3 py-2 text-sm text-muted-foreground leading-relaxed whitespace-pre-wrap',
              isThinking && 'italic'
            )}>
              {isThinking ? (
                <StreamingThinkingText content={thinking} />
              ) : (
                thinking
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
