import { cn } from "@/lib/utils";

export function Separator({
  className,
  orientation = "horizontal",
}: {
  className?: string;
  orientation?: "horizontal" | "vertical";
}) {
  return (
    <div
      role="separator"
      className={cn(
        "shrink-0 bg-border/70",
        orientation === "horizontal" ? "h-px w-full" : "h-full w-px",
        className
      )}
    />
  );
}
