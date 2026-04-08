import { File, FileText } from "lucide-react";

import { cn } from "@/lib/utils";
import type { Attachment } from "@/types";

type AttachmentCardVariant = "composer" | "message";

interface AttachmentCardProps {
  attachment: Attachment;
  variant: AttachmentCardVariant;
}

interface AttachmentTypePresentation {
  label: string;
  iconClassName?: string;
}

const ATTACHMENT_TYPE_PRESENTATION: Record<Attachment["type"], AttachmentTypePresentation> = {
  image: {
    label: "Image",
  },
  document: {
    label: "Document",
    iconClassName: "text-blue-400",
  },
  pdf: {
    label: "PDF",
    iconClassName: "text-red-400",
  },
  file: {
    label: "File",
  },
};

function attachmentPresentation(attachment: Attachment): AttachmentTypePresentation {
  return ATTACHMENT_TYPE_PRESENTATION[attachment.type];
}

export function AttachmentKindIcon({
  attachment,
  className,
}: {
  attachment: Attachment;
  className?: string;
}) {
  if (attachment.type === "image") {
    return null;
  }

  if (attachment.type === "document" || attachment.type === "pdf") {
    return (
      <FileText
        className={cn(
          "h-4 w-4 flex-shrink-0 text-muted-foreground",
          attachmentPresentation(attachment).iconClassName,
          className,
        )}
      />
    );
  }

  return <File className={cn("h-4 w-4 flex-shrink-0 text-muted-foreground", className)} />;
}

export function AttachmentCard({ attachment, variant }: AttachmentCardProps) {
  if (attachment.type === "image") {
    const className =
      variant === "composer"
        ? "h-16 w-16 border rounded-md overflow-hidden bg-muted"
        : "border rounded-md overflow-hidden bg-background";
    const imageClassName =
      variant === "composer"
        ? "h-full w-full object-cover"
        : "h-32 w-32 object-cover transition-transform hover:scale-105";
    return (
      <div className={className}>
        <img src={attachment.url} alt={attachment.name} className={imageClassName} />
      </div>
    );
  }

  if (attachment.type === "document" || attachment.type === "pdf") {
    const shellClassName =
      variant === "composer"
        ? "h-20 w-44 border rounded-lg overflow-hidden bg-muted/50 p-2.5 flex flex-col gap-1"
        : "h-20 w-48 border rounded-lg overflow-hidden bg-muted/40 p-2.5 flex flex-col gap-1";
    const bodyClassName =
      variant === "composer"
        ? "text-[10px] text-muted-foreground/70 leading-tight line-clamp-3 overflow-hidden"
        : "text-[10px] text-muted-foreground/70 leading-tight line-clamp-3";
    return (
      <div className={shellClassName}>
        <div className="flex items-center gap-1.5">
          <AttachmentKindIcon attachment={attachment} className="h-3.5 w-3.5" />
          <span className="text-[11px] font-medium truncate">{attachment.name}</span>
        </div>
        {attachment.textPreview ? (
          <p className={bodyClassName}>{attachment.textPreview}</p>
        ) : (
          <p className="mt-auto text-[10px] uppercase tracking-wide text-muted-foreground/50">
            {attachmentPresentation(attachment).label}
          </p>
        )}
      </div>
    );
  }

  if (variant === "composer") {
    return (
      <div className="flex h-16 w-16 items-center justify-center overflow-hidden rounded-md border bg-muted">
        <AttachmentKindIcon attachment={attachment} className="h-8 w-8" />
      </div>
    );
  }

  return (
    <div className="flex h-16 w-16 flex-col items-center justify-center gap-1 overflow-hidden rounded-md border bg-muted p-2">
      <AttachmentKindIcon attachment={attachment} className="h-6 w-6" />
      <span className="w-full truncate text-center text-[9px]" title={attachment.name}>
        {attachment.name}
      </span>
    </div>
  );
}
