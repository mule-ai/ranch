const PALETTE = (() => {
  const p = [
    "#000000",
    "#cd0000",
    "#00cd00",
    "#cdcd00",
    "#0000ee",
    "#cd00cd",
    "#00cdcd",
    "#e5e5e5",
    "#7f7f7f",
    "#ff0000",
    "#00ff00",
    "#ffff00",
    "#5c5cff",
    "#ff00ff",
    "#00ffff",
    "#ffffff"
  ];
  const steps = [0, 95, 135, 175, 215, 255];
  for (const r of steps) for (const g of steps) for (const b of steps) {
    p.push(`rgb(${r},${g},${b})`);
  }
  for (let i = 0; i < 24; i++) {
    const v = 8 + i * 10;
    p.push(`rgb(${v},${v},${v})`);
  }
  return p;
})();
const NAMED = {
  30: "#3b3b3b",
  31: "#cd3231",
  32: "#00bc00",
  33: "#949494",
  34: "#0451a5",
  35: "#bc05bc",
  36: "#0598bc",
  37: "#555555",
  90: "#7f7f7f",
  91: "#cd3131",
  92: "#14cc14",
  93: "#f5f543",
  94: "#3b78ff",
  95: "#d670d6",
  96: "#00a0a0",
  97: "#e5e5e5",
  40: "#3b3b3b",
  41: "#cd3231",
  42: "#00bc00",
  43: "#949494",
  44: "#0451a5",
  45: "#bc05bc",
  46: "#0598bc",
  47: "#555555",
  100: "#7f7f7f",
  101: "#cd3131",
  102: "#14cc14",
  103: "#f5f543",
  104: "#3b78ff",
  105: "#d670d6",
  106: "#00a0a0",
  107: "#e5e5e5"
};
function parseSgrRow(line) {
  const spans = [];
  let fg;
  let bg;
  let bold = false;
  let italic = false;
  let underline = false;
  let text = "";
  const flush = () => {
    if (text !== "") {
      spans.push({ text, fg, bg, bold, italic, underline });
      text = "";
    }
  };
  const applySgr = (params) => {
    let i2 = 0;
    while (i2 < params.length) {
      const p = params[i2];
      if (p === 0) {
        fg = bg = void 0;
        bold = italic = underline = false;
      } else if (p === 1) bold = true;
      else if (p === 2) bold = false;
      else if (p === 3) italic = true;
      else if (p === 4) underline = true;
      else if (p === 22) bold = false;
      else if (p === 23) italic = false;
      else if (p === 24) underline = false;
      else if (p === 39) fg = void 0;
      else if (p === 49) bg = void 0;
      else if (NAMED[p] !== void 0) {
        if (p >= 40 && p <= 47) bg = NAMED[p];
        else if (p >= 100 && p <= 107) bg = NAMED[p];
        else fg = NAMED[p];
      } else if (p === 38 || p === 48) {
        const isFg = p === 38;
        if (params[i2 + 1] === 5) {
          const idx = params[i2 + 2] ?? 0;
          const color = PALETTE[idx] ?? "#cccccc";
          if (isFg) fg = color;
          else bg = color;
          i2 += 2;
        } else if (params[i2 + 1] === 2) {
          const r = params[i2 + 2] ?? 0, g = params[i2 + 3] ?? 0, b = params[i2 + 4] ?? 0;
          const color = `rgb(${r},${g},${b})`;
          if (isFg) fg = color;
          else bg = color;
          i2 += 4;
        }
      }
      i2++;
    }
  };
  let i = 0;
  while (i < line.length) {
    const ch = line[i];
    if (ch === "\x1B" && line[i + 1] === "[") {
      let j = i + 2;
      let paramsStr = "";
      while (j < line.length && line[j] >= "0" && line[j] <= ";") {
        paramsStr += line[j];
        j++;
      }
      if (line[j] === "m") {
        flush();
        const params = paramsStr === "" ? [0] : paramsStr.split(";").map((x) => parseInt(x || "0", 10) || 0);
        applySgr(params);
        i = j + 1;
        continue;
      }
      flush();
      while (j < line.length && !(line[j] >= "@" && line[j] <= "~")) j++;
      i = j + 1;
      continue;
    }
    text += ch;
    i++;
  }
  flush();
  return spans;
}
export {
  parseSgrRow
};
