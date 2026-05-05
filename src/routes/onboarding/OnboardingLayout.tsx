import { Outlet } from "react-router-dom";
import { BrainIcon } from "../../components/ui/BrainIcon";

export function OnboardingLayout() {
  return (
    <div className="flex min-h-screen items-center justify-center p-6 lg:p-10">
      <div className="w-full max-w-3xl rounded-xl border border-neutral-800 bg-neutral-900 p-8 shadow-2xl xl:max-w-4xl">
        <div className="mb-6 flex items-center gap-2 text-sm text-neutral-500">
          <BrainIcon size={22} />
          <span className="font-semibold text-neutral-300">BRAIN</span>
          <span>·</span>
          <span>Setup</span>
        </div>
        <Outlet />
      </div>
    </div>
  );
}
