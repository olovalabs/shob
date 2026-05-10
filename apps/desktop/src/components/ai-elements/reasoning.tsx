import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Markdown } from "@/components/shob/tools/markdown";
import { cn } from "@/lib/utils";
import { BrainIcon, ChevronDownIcon } from "lucide-react";
import type { ComponentProps, ReactNode } from "react";
import {
  createContext,
  memo,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { Shimmer } from "./shimmer";

interface ReasoningContextValue {
  isStreaming: boolean;
  isOpen: boolean;
  setIsOpen: (open: boolean) => void;
  duration: number | undefined;
}

const ReasoningContext = createContext<ReasoningContextValue | null>(null);

const useReasoning = () => {
  const context = useContext(ReasoningContext);
  if (!context) {
    throw new Error("Reasoning components must be used within Reasoning");
  }
  return context;
};

export type ReasoningProps = ComponentProps<typeof Collapsible> & {
  isStreaming?: boolean;
  duration?: number;
};

const AUTO_CLOSE_DELAY = 1000;
const MS_IN_S = 1000;

const ReasoningComponent = ({
    className,
    isStreaming = false,
    open: openProp,
    defaultOpen,
    onOpenChange,
    duration: durationProp,
    children,
    ...props
  }: ReasoningProps) => {
    const [userOpen, setUserOpen] = useState(defaultOpen ?? isStreaming);
    const [measuredDuration, setMeasuredDuration] = useState<number | undefined>(undefined);

    const hasEverStreamedRef = useRef(isStreaming);
    const [hasAutoClosed, setHasAutoClosed] = useState(false);
    const startTimeRef = useRef<number | null>(null);
    const baseOpen = openProp ?? userOpen;
    const isOpen = isStreaming || baseOpen;
    const duration = durationProp ?? measuredDuration;

    // Track streaming duration
    useEffect(() => {
      if (isStreaming) {
        hasEverStreamedRef.current = true;
        if (startTimeRef.current === null) {
          startTimeRef.current = Date.now();
        }
      } else if (startTimeRef.current !== null) {
        const nextDuration = Math.ceil((Date.now() - startTimeRef.current) / MS_IN_S);
        startTimeRef.current = null;

        const frame = requestAnimationFrame(() => setMeasuredDuration(nextDuration));
        return () => cancelAnimationFrame(frame);
      }
    }, [isStreaming]);

    // Auto-close after streaming ends
    useEffect(() => {
      if (
        hasEverStreamedRef.current &&
        !isStreaming &&
        isOpen &&
        !hasAutoClosed
      ) {
        const timer = setTimeout(() => {
          setUserOpen(false);
          setHasAutoClosed(true);
          onOpenChange?.(false);
        }, AUTO_CLOSE_DELAY);

        return () => clearTimeout(timer);
      }
    }, [isStreaming, isOpen, hasAutoClosed, onOpenChange]);

    const handleOpenChange = useCallback((nextOpen: boolean) => {
      setUserOpen(nextOpen);
      onOpenChange?.(nextOpen);
    }, [onOpenChange]);

    const contextValue = useMemo(
      () => ({ duration, isOpen, isStreaming, setIsOpen: handleOpenChange }),
      [duration, isOpen, isStreaming, handleOpenChange]
    );

    return (
      <ReasoningContext.Provider value={contextValue}>
        <Collapsible
          className={cn("not-prose my-2", className)}
          onOpenChange={handleOpenChange}
          open={isOpen}
          {...props}
        >
          {children}
        </Collapsible>
      </ReasoningContext.Provider>
    );
  };

export const Reasoning = memo(ReasoningComponent);

export type ReasoningTriggerProps = ComponentProps<
  typeof CollapsibleTrigger
> & {
  getThinkingMessage?: (isStreaming: boolean, duration?: number) => ReactNode;
};

const defaultGetThinkingMessage = (isStreaming: boolean, duration?: number) => {
  if (isStreaming || duration === 0) {
    return <Shimmer duration={1}>Thinking...</Shimmer>;
  }
  if (duration === undefined) {
    return "Thought for a few seconds";
  }
  return `Thought for ${duration} seconds`;
};

export const ReasoningTrigger = memo(
  ({
    className,
    children,
    getThinkingMessage = defaultGetThinkingMessage,
    ...props
  }: ReasoningTriggerProps) => {
    const { isStreaming, duration } = useReasoning();

    return (
      <CollapsibleTrigger
        className={cn(
          "group flex w-full cursor-pointer items-center gap-2 text-muted-foreground text-sm transition-colors hover:text-foreground",
          className
        )}
        {...props}
      >
        {children ?? (
          <>
            <BrainIcon className="size-4" />
            <div className="text-sm">{getThinkingMessage(isStreaming, duration)}</div>
            <ChevronDownIcon
              className={cn(
                "size-4 transition-transform group-data-[state=open]:rotate-180",
              )}
            />
          </>
        )}
      </CollapsibleTrigger>
    );
  }
);

export type ReasoningContentProps = ComponentProps<
  typeof CollapsibleContent
> & {
  children: string;
};

const ReasoningContentComponent = ({ className, children, ...props }: ReasoningContentProps) => {
  const { isStreaming } = useReasoning();
  
  return (
    <CollapsibleContent
      className={cn(
        // Disable enter/exit animations while streaming to prevent flickering
        !isStreaming && "data-[state=closed]:animate-out data-[state=open]:animate-in data-[state=closed]:fade-out-0 data-[state=closed]:slide-out-to-top-2 data-[state=open]:slide-in-from-top-2",
        "outline-none",
        className
      )}
      {...props}
    >
      <div className="mt-4 space-y-2 border-muted border-l-2 pl-4 text-muted-foreground text-sm">
        <Markdown text={children} streaming={isStreaming} />
      </div>
    </CollapsibleContent>
  );
};

export const ReasoningContent = memo(
  ReasoningContentComponent,
  (prev, next) => prev.children === next.children && prev.className === next.className
);

Reasoning.displayName = "Reasoning";
ReasoningTrigger.displayName = "ReasoningTrigger";
ReasoningContent.displayName = "ReasoningContent";
