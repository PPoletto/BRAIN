import { createContext, useContext } from "react";

export type ToastKind = "info" | "success" | "warning" | "error";

export type ToastInput = {
  kind: ToastKind;
  message: string;
  detail?: string;
};

export type ToastContextValue = {
  push: (toast: ToastInput) => string;
  dismiss: (id: string) => void;
};

export const ToastContext = createContext<ToastContextValue | null>(null);

export function useToast() {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be inside <ToastProvider>");
  return ctx;
}
