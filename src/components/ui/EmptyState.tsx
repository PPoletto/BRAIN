import type { ReactNode } from "react";

type Props = {
  title: string;
  description?: string;
  icon?: ReactNode;
  action?: ReactNode;
};

export function EmptyState({ title, description, icon, action }: Props) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 p-12 text-center">
      {icon && <div className="text-4xl text-neutral-600">{icon}</div>}
      <div className="text-base font-medium text-neutral-200">{title}</div>
      {description && (
        <div className="max-w-md text-sm text-neutral-500">{description}</div>
      )}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}
