#!/usr/bin/env python3
"""Regenerate installer/License.rtf (the licence page of the setup) from LICENSE.

    python installer/make-license-rtf.py

Paragraphs of the plain-text licence are re-flowed (the text is hard-wrapped at about 70 columns) and
set in Segoe UI. Only ASCII is expected; anything else is written as an RTF unicode escape.
"""
import pathlib

root = pathlib.Path(__file__).resolve().parent.parent
text = (root / "LICENSE").read_text(encoding="utf-8").replace("\r\n", "\n")


def esc(s: str) -> str:
    out = []
    for ch in s:
        if ch in "\\{}":
            out.append("\\" + ch)
        elif ord(ch) < 128:
            out.append(ch)
        else:
            out.append("\\u%d?" % (ord(ch) if ord(ch) < 32768 else ord(ch) - 65536))
    return "".join(out)


paras, cur = [], []
for line in text.split("\n"):
    if line.strip() == "":
        if cur:
            paras.append(cur)
            cur = []
    else:
        cur.append(line)
if cur:
    paras.append(cur)

rtf = ["{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fswiss\\fcharset0 Segoe UI;}}\\viewkind4\\uc1\\pard\\f0\\fs18"]
for p in paras:
    first = p[0]
    indent = len(first) - len(first.lstrip(" "))
    body = " ".join(l.strip() for l in p)
    # the title block is centred, other indented paragraphs are indented a little
    if indent >= 15 and len(p) <= 2:
        rtf.append("\\pard\\qc\\sa120 " + esc(body) + "\\par")
    elif indent >= 2:
        rtf.append("\\pard\\li360\\sa120 " + esc(body) + "\\par")
    else:
        rtf.append("\\pard\\sa120 " + esc(body) + "\\par")
rtf.append("}")
(root / "installer" / "License.rtf").write_text("\n".join(rtf) + "\n", encoding="ascii")
print("wrote installer/License.rtf (%d paragraphs)" % len(paras))
