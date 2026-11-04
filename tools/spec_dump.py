"""Dump the spec DOCX to plain text with heading markers. Stdlib only."""
import sys, zipfile, re
import xml.etree.ElementTree as ET

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"

def para_style(p):
    pPr = p.find(W + "pPr")
    if pPr is None:
        return None, None
    st = pPr.find(W + "pStyle")
    style = st.get(W + "val") if st is not None else None
    npr = pPr.find(W + "numPr")
    numbered = npr is not None
    return style, numbered

def text_of(p):
    out = []
    for node in p.iter():
        t = node.tag
        if t == W + "t":
            out.append(node.text or "")
        elif t == W + "tab":
            out.append("\t")
        elif t in (W + "br", W + "cr"):
            out.append("\n")
    return "".join(out)

def emit_table(tbl, lines):
    for tr in tbl.findall(W + "tr"):
        cells = []
        for tc in tr.findall(W + "tc"):
            ct = " ".join(text_of(p).strip() for p in tc.findall(W + "p")).strip()
            cells.append(ct.replace("|", "/"))
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("")

def main(src, dst):
    with zipfile.ZipFile(src) as z:
        root = ET.fromstring(z.read("word/document.xml"))
    body = root.find(W + "body")
    lines = []
    for el in body:
        if el.tag == W + "p":
            style, numbered = para_style(el)
            txt = text_of(el).strip()
            if not txt:
                continue
            m = re.match(r"Heading(\d+)|H(\d+)", style or "")
            lvl = None
            if m:
                lvl = int(m.group(1) or m.group(2))
            elif style in ("Title",):
                lvl = 1
            if lvl:
                lines.append("#" * min(lvl, 6) + " " + txt)
            elif numbered:
                lines.append("- " + txt)
            else:
                lines.append(txt)
                lines.append("")
        elif el.tag == W + "tbl":
            emit_table(el, lines)
    with open(dst, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"wrote {dst} ({len(lines)} lines)")

if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
