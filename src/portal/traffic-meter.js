for (const facts of document.querySelectorAll('dl[aria-label="账户流量"]')) {
  const bytes = label => {
    const term = [...facts.querySelectorAll("dt")].find(node => node.textContent === label);
    const count = term?.nextElementSibling?.title.match(/^(\d+) 字节$/);
    return count ? Number(count[1]) : NaN;
  };

  const used = bytes("本月已用");
  const limit = bytes("本月额度");
  if (!Number.isFinite(used)) continue;

  const panel = document.createElement("section");
  panel.className = "traffic-summary";
  panel.setAttribute("aria-label", "本月流量");
  facts.before(panel);
  panel.append(facts);

  const note = panel.nextElementSibling;
  if (note?.matches(".dashboard-note")) panel.append(note);

  if (!Number.isFinite(limit) || limit <= 0) continue;

  const meter = document.createElement("progress");
  meter.className = "traffic-meter";
  meter.max = limit;
  meter.value = Math.min(used, limit);
  meter.setAttribute("aria-label", "本月流量用量");
  const ratio = Math.min(used / limit * 100, 100);
  const percent = ratio === 100 ? "100%" :
    ratio >= 99.95 ? ">99.9%" : ratio > 0 && ratio < 0.1 ? "<0.1%" : `${Number(ratio.toFixed(1))}%`;
  meter.setAttribute("aria-valuetext", `已用 ${percent}`);
  const label = document.createElement("p");
  label.className = "traffic-meter-caption";
  label.textContent = `已用 ${percent}`;
  facts.after(meter, label);
}
