// Numeric palette indices and packed RGB values come from parsed PTY cells.
const ansi = ['#000000', '#cd3131', '#0dbc79', '#e5e510', '#2472c8', '#bc3fbc', '#11a8cd', '#e5e5e5',
  '#666666', '#f14c4c', '#23d18b', '#f5f543', '#3b8eea', '#d670d6', '#29b8db', '#ffffff'];
const levels = [0, 95, 135, 175, 215, 255];
function color(value, fallback) {
  if (!Number.isInteger(value)) return fallback;
  if (value >= 0x1000000) return '#' + (value & 0xffffff).toString(16).padStart(6, '0');
  if (value < 16) return ansi[value];
  if (value >= 232) {
    const n = 8 + 10 * (value - 232);
    return `rgb(${n},${n},${n})`;
  }
  const n = value - 16;
  return `rgb(${levels[Math.floor(n / 36)]},${levels[Math.floor(n / 6) % 6]},${levels[n % 6]})`;
}

function buildRun([columns, text, { fg, bg, flags }]) {
  const node = document.createElement('span');
  node.textContent = text;
  node.style.width = `calc(var(--cell-width) * ${columns})`;
  if (fg != null || flags & 16) node.style.color = flags & 16 ? color(bg, '#000') : color(fg, '');
  if (bg != null || flags & 16) node.style.backgroundColor = flags & 16 ? color(fg, '#e3e3e3') : color(bg, '');
  if (flags & 1) node.style.fontWeight = 'bold';
  if (flags & 2) node.style.opacity = '.75';
  if (flags & 4) node.style.fontStyle = 'italic';
  if (flags & 8) node.style.textDecoration = 'underline';
  return node;
}

// Preserve unchanged DOM rows during typing, selection and color-only updates.
export function drawFrame(screen, snapshot) {
  if (!Array.isArray(snapshot.rows)) {
    if (screen.textContent === snapshot.screen && !screen.firstElementChild) return false;
    screen.textContent = snapshot.screen;
    return true;
  }
  if (!screen.firstElementChild && screen.firstChild) screen.replaceChildren();
  let changed = false;
  for (const [index, runs] of snapshot.rows.entries()) {
    let row = screen.children[index];
    const key = JSON.stringify(runs);
    if (row && row.frameKey === key) continue;
    if (!row) {
      row = document.createElement('div');
      row.className = 'terminal-row';
      screen.append(row);
    }
    row.replaceChildren(...runs.map(buildRun));
    row.frameKey = key;
    changed = true;
  }
  while (screen.childElementCount > snapshot.rows.length) { screen.lastElementChild.remove(); changed = true; }
  return changed;
}
