#!/usr/bin/env python3
"""Build self-contained HTML copies of the install guides (images embedded) into the folder given as argv[1]."""
import base64, re, sys, pathlib, markdown
root = pathlib.Path(__file__).resolve().parent.parent
out = pathlib.Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
CSS = """body{max-width:980px;margin:32px auto;padding:0 20px;font:16px/1.55 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;color:#1d2230;background:#fff}
img{max-width:100%;height:auto;border:1px solid #d6dae3;border-radius:8px;margin:8px 0}code{background:#eef0f5;padding:1px 5px;border-radius:4px}
pre{background:#14161c;color:#e3e6ee;padding:12px 14px;border-radius:8px;overflow:auto}pre code{background:none;color:inherit;padding:0}
table{border-collapse:collapse;width:100%;font-size:14.5px}td,th{border:1px solid #d6dae3;padding:6px 9px;vertical-align:top;text-align:left}th{background:#f1f3f8}
blockquote{border-left:4px solid #5865f2;margin:12px 0;padding:2px 14px;background:#f5f6ff}h1{font-size:30px}h2{margin-top:36px;border-bottom:1px solid #d6dae3;padding-bottom:4px}"""
for name in ("INSTALL-LINUX", "INSTALL-WINDOWS", "GITHUB-GUIDE"):
    md = (root / f"{name}.md").read_text(encoding="utf-8")
    def emb(m):
        p = root / m.group(2)
        b64 = base64.b64encode(p.read_bytes()).decode()
        return f'![{m.group(1)}](data:image/png;base64,{b64})'
    md = re.sub(r'!\[([^\]]*)\]\((docs/img/[^)]+)\)', emb, md)
    body = markdown.markdown(md, extensions=["tables", "fenced_code", "sane_lists"])
    title = md.splitlines()[0].lstrip("# ").strip()
    (out / f"{name}.html").write_text(f'<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>{CSS}</style>{body}', encoding="utf-8")
    print(out / f"{name}.html")
