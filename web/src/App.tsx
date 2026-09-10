import { useRoute } from "./lib/router";
import { Landing } from "./pages/Landing";
import { Download } from "./pages/Download";
import { WebApp } from "./pages/WebApp";

export default function App() {
  const [path] = useRoute();
  if (path.startsWith("/download")) return <Download />;
  if (path.startsWith("/app")) return <WebApp />;
  return <Landing />;
}
