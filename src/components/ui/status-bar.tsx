import { Loader2, CircleCheck, CircleAlert, Info } from "lucide-react";
import { cn } from "@/lib/utils";

export type StatusVariant = "info" | "ok" | "err";

interface StatusBarProps {
  variant: StatusVariant;
  message: string;
  spinner?: boolean;
  className?: string;
}

const VARIANT_STYLES: Record<StatusVariant, string> = {
  info: "border-primary/30 bg-primary/10 text-primary",
  ok: "border-success/30 bg-success/10 text-success",
  err: "border-destructive/30 bg-destructive/10 text-destructive",
};

const ICONS: Record<StatusVariant, React.ComponentType<{ className?: string }>> = {
  info: Info,
  ok: CircleCheck,
  err: CircleAlert,
};

export function StatusBar({ variant, message, spinner, className }: StatusBarProps) {
  const Icon = ICONS[variant];
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-md border px-3 py-2 text-xs leading-relaxed",
        VARIANT_STYLES[variant],
        className
      )}
    >
      {spinner ? (
        <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin" />
      ) : (
        <Icon className="h-3.5 w-3.5 shrink-0" />
      )}
      <span className="break-words">{message}</span>
    </div>
  );
}
