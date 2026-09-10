// Tiny hash router: "#/" landing, "#/download", "#/app" (web client).
// Hash routing keeps deep links working on GitHub Pages (no rewrites).
import { useState, useEffect, useCallback } from "react";

export type Route = { path: string };

const read = () => window.location.hash.replace(/^#/, "") || "/";

export function useRoute(): [string, (to: string) => void] {
  const [path, setPath] = useState(read);
  useEffect(() => {
    const on = () => setPath(read());
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  const nav = useCallback((to: string) => {
    window.location.hash = to;
    window.scrollTo(0, 0);
  }, []);
  return [path, nav];
}

export function Link({ to, children, className }: { to: string; children: React.ReactNode; className?: string }) {
  return (
    <a
      href={`#${to}`}
      className={className}
      onClick={(e) => {
        e.preventDefault();
        window.location.hash = to;
      }}
    >
      {children}
    </a>
  );
}
