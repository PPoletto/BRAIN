import { createBrowserRouter } from "react-router-dom";
import { Bootstrap } from "./routes/Bootstrap";
import { AppShell } from "./components/shell/AppShell";
import { OnboardingLayout } from "./routes/onboarding/OnboardingLayout";
import { Welcome } from "./routes/onboarding/Welcome";
import { Medium } from "./routes/onboarding/Medium";
import { Format } from "./routes/onboarding/Format";
import { TemplatePopulation } from "./routes/onboarding/TemplatePopulation";
import { Completion } from "./routes/onboarding/Completion";
import { Settings } from "./routes/settings/Settings";
import { ViewerLayout } from "./routes/viewer/ViewerLayout";
import { Tier1 } from "./routes/viewer/Tier1";
import { Tier2 } from "./routes/viewer/Tier2";
import { Tier3 } from "./routes/viewer/Tier3";
import { WikiHistory } from "./routes/wiki-history/WikiHistory";
import { Integrity } from "./routes/integrity/Integrity";

export const router = createBrowserRouter([
  { path: "/", element: <Bootstrap /> },
  {
    path: "/onboarding",
    element: <OnboardingLayout />,
    children: [
      { index: true, element: <Welcome /> },
      { path: "medium", element: <Medium /> },
      { path: "format", element: <Format /> },
      { path: "template", element: <TemplatePopulation /> },
      { path: "completion", element: <Completion /> },
    ],
  },
  {
    // All mounted-Brain routes share the AppShell (TopBar + Sidebar + StatusBar).
    element: <AppShell />,
    children: [
      { path: "/settings", element: <Settings /> },
      {
        path: "/viewer",
        element: <ViewerLayout />,
        children: [
          { index: true, element: <Tier1 /> },
          { path: "tier2", element: <Tier2 /> },
          { path: "graph", element: <Tier3 /> },
        ],
      },
      { path: "/wiki-history", element: <WikiHistory /> },
      { path: "/integrity", element: <Integrity /> },
    ],
  },
]);
