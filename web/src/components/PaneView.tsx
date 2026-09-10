// Web terminal pane: renders one pane's rows as a <pre> of styled spans
// (same SGR row parsing as mobile), absolutely positioned inside the
// split-tree rect — the web analog of Terminal.tsx's pane renderer.
import { useMemo } from "react";
import { PaneSnap } from "../lib/frames";
import { parseSgrRow, Span as SgrSpan } from "../lib/sgr";

export type Rect = { pane: string; x: number; y: number; w: number; h: number };

const FS = 13; // px font size; JetBrains Mono advance = 0.6em
const LH = 17;

export { FS, LH };

type Props = {
  pane: PaneSnap;
  rect: Rect;
  cols: number;
  rows: number;
  focused: boolean;
  onSelect: () => void;
};

export function PaneView({ pane, rect, cols, rows, focused, onSelect }: Props) {
  const lines = pane.lines.slice(0, rect.h);
  const cursor = focused ? pane.cursor : undefined;

  const body = useMemo(
    () =>
      lines.map((line, y) => {
        const spans = parseSgrRow(line);
        const cur = cursor && y === cursor.y ? cursor : undefined;
        return <TermRow key={y} spans={spans} cur={cur} />;
      }),
    [lines, cursor]
  );

  return (
    <div
      className={"pane" + (focused ? " pane-focused" : "")}
      onMouseDown={onSelect}
      style={{
        left: `${(rect.x / cols) * 100}%`,
        top: `${(rect.y / rows) * 100}%`,
        width: `${(rect.w / cols) * 100}%`,
        height: `${(rect.h / rows) * 100}%`,
        fontSize: FS,
        lineHeight: `${LH}px`,
      }}
    >
      <pre className="pane-pre">{body}</pre>
    </div>
  );
}

function TermRow({ spans, cur }: { spans: SgrSpan[]; cur?: { x: number; y: number } }) {
  const out: React.ReactNode[] = [];
  let col = 0;
  let done = !cur;
  spans.forEach((sp, si) => {
    const chars = sp.text === "" ? [""] : [...sp.text];
    for (let k = 0; k < chars.length; k++) {
      if (!done && col === cur!.x) {
        out.push(
          <span key={`c${si}-${k}`} className="cursor-cell" style={spanCss(sp)}>
            {chars[k] === "" ? " " : chars[k]}
          </span>
        );
        done = true;
      } else {
        out.push(
          <span key={`${si}-${k}`} style={spanCss(sp)}>
            {chars[k]}
          </span>
        );
      }
      col++;
    }
  });
  if (!done) out.push(<span key="cend" className="cursor-cell"> </span>);
  return <div>{out.length ? out : "\u00a0"}</div>;
}

function spanCss(sp: SgrSpan): React.CSSProperties {
  return {
    color: sp.fg,
    background: sp.bg,
    fontWeight: sp.bold ? 700 : undefined,
    fontStyle: sp.italic ? "italic" : undefined,
    textDecoration: sp.underline ? "underline" : undefined,
  };
}
