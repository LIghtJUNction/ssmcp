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
a=p.parse_args()
text=a.ansi.read_text()
colors={'0':'#d2d7de','31':'#f28b82','32':'#81c995','33':'#fdd663','34':'#8ab4f8','35':'#c58af9','36':'#78d9ec','90':'#80868b'}
parts=[]
color=colors['0']
for part in re.split(r'(\x1b\[[0-9;]*m)',text):
    if part.startswith('\x1b['):
        color=colors.get(part[2:-1].split(';')[0],colors['0'])
    else:
        parts.append('<span style="color:'+color+'">'+html.escape(part)+'</span>')
height=max(480,170+len(text.splitlines())*27)
page="""<!doctype html><meta charset="utf-8"><style>
* {box-sizing:border-box} body {margin:0;background:#101214;color:#d2d7de;padding:40px 48px}
header {font:500 14px system-ui;letter-spacing:.14em;color:#80868b;margin-bottom:26px}
pre {font:17px/27px "DejaVu Sans Mono",monospace;margin:0;white-space:pre-wrap}
footer {font:12px system-ui;color:#80868b;margin-top:28px}
</style><header>SSMCP / PRETTY LOGS</header><pre>"""+''.join(parts)+'</pre><footer>Renderer fixture · Request / result / stdout / stderr / progress / timeout</footer>'
a.output=a.output.resolve();a.output.parent.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(prefix='ssmcp-preview-') as temp:
    root=Path(temp);source=root/'preview.html';source.write_text(page)
    subprocess.run([a.browser,'--headless','--disable-gpu','--hide-scrollbars',
        '--no-sandbox','--user-data-dir='+str(root/'profile'),'--screenshot='+str(a.output),
        '--window-size=1280,'+str(height),source.as_uri()],check=True,capture_output=True,timeout=45)
print(a.output)
