import { useCallback, useRef, useState } from "react";
import { useToast } from "./toast-context";

type Options = {
  /** Toast pushed while the action is running. Pass `null` to suppress.
   *  Default: undefined → no progress toast. */
  pending?: string | null;
  /** Toast pushed on success. Pass `null` to suppress. Default: undefined → none. */
  success?: string | null;
  /** Prefix for the error-toast message. Default: "Action failed". */
  errorPrefix?: string;
};

/**
 * Wraps an async handler so the UI gets immediate feedback:
 *
 * - `loading` flips to true the moment the user clicks, false when the
 *   promise settles. Buttons read this through their `loading` prop and
 *   render a spinner so the click never feels lost.
 * - On rejection, a red toast pops with the error message — the user
 *   doesn't have to dig through logs to find out something went wrong.
 * - Optional `pending` and `success` toasts give heavier operations
 *   (Reset, Re-register, Refresh disks) a "started → done" arc.
 *
 * Subsequent clicks while a previous run is in flight are ignored, so
 * double-clicks don't fan out into duplicate work.
 */
export function useAsyncAction<TArgs extends unknown[], TResult>(
  fn: (...args: TArgs) => Promise<TResult>,
  options: Options = {},
): {
  trigger: (...args: TArgs) => Promise<TResult | undefined>;
  loading: boolean;
} {
  const [loading, setLoading] = useState(false);
  const inFlight = useRef(false);
  const { push } = useToast();

  const trigger = useCallback(
    async (...args: TArgs) => {
      if (inFlight.current) return undefined;
      inFlight.current = true;
      setLoading(true);
      let pendingId: string | undefined;
      if (options.pending) {
        pendingId = push({ kind: "info", message: options.pending });
      }
      try {
        const result = await fn(...args);
        if (options.success) {
          push({ kind: "success", message: options.success });
        }
        return result;
      } catch (e: unknown) {
        const detail = e instanceof Error ? e.message : String(e);
        push({
          kind: "error",
          message: options.errorPrefix ?? "Action failed",
          detail,
        });
        return undefined;
      } finally {
        if (pendingId) {
          // The pending toast auto-dismisses on a 4 s timer; we don't need
          // to clean it up explicitly. Keeping the dismiss-on-finish call
          // commented to avoid an out-of-order race with the pending push.
        }
        inFlight.current = false;
        setLoading(false);
      }
    },
    [fn, options.pending, options.success, options.errorPrefix, push],
  );

  return { trigger, loading };
}
