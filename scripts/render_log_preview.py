#!/usr/bin/env python3
"""Render the actual ANSI test transcript to a reproducible README screenshot."""
import argparse
import html
from pathlib import Path
import re
import subprocess
import tempfile

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('ansi',type=Path)
p.add_argument('--output',type=Path,default=Path('docs/assets/terminal-logs.png'))
p.add_argument('--browser',default='chromium')
p.add_argument('--columns',type=int,default=100,
    help='Terminal columns used by the renderer; browser wrapping is disabled')
a=p.parse_args()
if not 12 <= a.columns <= 300:
    p.error('--columns must be between 12 and 300')
text=a.ansi.read_text()
colors={'0':'#d2d7de','31':'#f28b82','32':'#81c995','33':'#fdd663','34':'#8ab4f8','35':'#c58af9','36':'#78d9ec','90':'#80868b'}
parts=[]
color=colors['0']
for part in re.split(r'(\x1b\[[0-9;]*m)',text):
    if part.startswith('\x1b['):
        color=colors.get(part[2:-1].split(';')[0],colors['0'])
    else:
        parts.append('<span style="color:'+color+'">'+html.escape(part)+'</span>')
page="""<!doctype html><meta charset="utf-8"><style>
* {box-sizing:border-box} body {margin:0;background:#101214;color:#d2d7de;padding:40px 48px;
font:17px/27px "DejaVu Sans Mono",monospace;width:calc(__COLS__ * 1ch + 96px)}
header {font:500 14px system-ui;letter-spacing:.14em;color:#80868b;margin-bottom:26px}
pre {font:inherit;margin:0;white-space:pre}
footer {font:12px system-ui;color:#80868b;margin-top:28px}
</style><header>SSMCP / PRETTY LOGS · __COLS__ COLUMNS</header><pre>""".replace('__COLS__',str(a.columns))
page+=''.join(parts)+'</pre><footer>Renderer fixture · Replayed events; no commands executed.</footer>'
page+='''<script>
const box = document.body.getBoundingClientRect();
document.body.dataset.captureWidth = Math.ceil(box.width);
document.body.dataset.captureHeight = Math.ceil(box.height);
</script>'''
a.output=a.output.resolve();a.output.parent.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(prefix='ssmcp-preview-') as temp:
    root=Path(temp);source=root/'preview.html';source.write_text(page)
    browser=[a.browser,'--headless','--disable-gpu','--hide-scrollbars',
        '--no-sandbox','--user-data-dir='+str(root/'profile')]
    measured=subprocess.run(browser+['--dump-dom',source.as_uri()],
        check=True,capture_output=True,text=True,timeout=45).stdout
    width=re.search(r'data-capture-width="(\d+)"',measured)
    height=re.search(r'data-capture-height="(\d+)"',measured)
    if width is None or height is None:
        raise RuntimeError('Browser did not return rendered transcript dimensions')
    size=f'{width.group(1)},{height.group(1)}'
    subprocess.run(browser+['--screenshot='+str(a.output),'--window-size='+size,source.as_uri()],
        check=True,capture_output=True,timeout=45)
print(a.output)
