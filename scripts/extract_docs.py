#!/usr/bin/env python3
"""Regenerate catalog/docs.json from the Lexware API documentation.

Dev-only helper (stdlib only). Usage:
    curl -sSL -o /tmp/lx/public.html  https://developers.lexware.io/docs/
    curl -sSL -o /tmp/lx/partner.html https://developers.lexware.io/partner/docs/
    python3 -I scripts/extract_docs.py /tmp/lx/public.html /tmp/lx/partner.html catalog/docs.json

The output holds, per docs section anchor, the property tables (field reference),
the required-field tables of create/update sections, request/response samples and
the section text (used by the catalog coverage test). Docs URLs are derived from the
anchor at runtime, so they are not stored.
"""
import json
import re
import sys
from html.parser import HTMLParser


class Blocks(HTMLParser):
    """Splits a Slate docs page into sections of ordered blocks (p, li, table, code)."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.sections = []
        self.cur = None
        self.skip = 0
        self.heading = None
        self.heading_text = ""
        self.heading_id = ""
        self.table = None
        self.row = None
        self.cell = None
        self.pre = None
        self.para = None
        self.li = None

    def add(self, block):
        if self.cur is not None:
            self.cur["blocks"].append(block)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag in ("script", "style"):
            self.skip += 1
        elif tag in ("h1", "h2", "h3"):
            self.heading = int(tag[1])
            self.heading_text = ""
            self.heading_id = attrs.get("id", "")
        elif tag == "table":
            self.table = []
        elif tag == "tr" and self.table is not None:
            self.row = []
        elif tag in ("td", "th") and self.row is not None:
            self.cell = ""
        elif tag == "pre":
            self.pre = ""
        elif tag == "p" and self.table is None:
            self.para = ""
        elif tag == "li" and self.table is None:
            self.li = ""
        elif tag == "br" and self.cell is not None:
            self.cell += "\n"

    def handle_endtag(self, tag):
        if tag in ("script", "style"):
            self.skip -= 1
        elif tag in ("h1", "h2", "h3") and self.heading:
            self.cur = {
                "level": self.heading,
                "id": self.heading_id,
                "title": " ".join(self.heading_text.split()),
                "blocks": [],
            }
            self.sections.append(self.cur)
            self.heading = None
        elif tag == "table" and self.table is not None:
            self.add({"t": "table", "rows": self.table})
            self.table = None
        elif tag == "tr" and self.row is not None:
            self.table.append(self.row)
            self.row = None
        elif tag in ("td", "th") and self.cell is not None:
            lines = (" ".join(x.split()) for x in self.cell.split("\n"))
            self.row.append("\n".join(l for l in lines if l))
            self.cell = None
        elif tag == "pre" and self.pre is not None:
            self.add({"t": "code", "text": self.pre})
            self.pre = None
        elif tag == "p" and self.para is not None:
            if self.para.strip():
                self.add({"t": "p", "text": " ".join(self.para.split())})
            self.para = None
        elif tag == "li" and self.li is not None:
            if self.li.strip():
                self.add({"t": "li", "text": " ".join(self.li.split())})
            self.li = None

    def handle_data(self, data):
        if self.skip:
            return
        if self.heading:
            self.heading_text += data
        elif self.pre is not None:
            self.pre += data
        elif self.cell is not None:
            self.cell += data
        elif self.para is not None:
            self.para += data
        elif self.li is not None:
            self.li += data


def parse(path):
    parser = Blocks()
    parser.feed(open(path, encoding="utf-8").read())
    return parser.sections


OBJECT_NAME = re.compile(r"^(?:Object )?(.+?) (?:Details|Required Properties|Properties)\b")


def object_name(text):
    m = OBJECT_NAME.match(text)
    return m.group(1).strip() if m else None


def field_row(row, required_table):
    head = row[0].split("\n")
    name = head[0].strip()
    ftype = head[1].strip() if len(head) > 1 else None
    if required_table:
        return {
            "name": name,
            "required": row[1] if len(row) > 1 else "",
            "notes": row[2] if len(row) > 2 else "",
        }
    desc = " ".join(row[1:]).replace("\n", " ").strip()
    out = {"name": name, "type": ftype, "description": desc}
    if re.search(r"\bread-only\b", desc, re.I):
        out["readOnly"] = True
    return out


def tables_of(section, required_tables):
    """Group the section's tables into named objects using the preceding paragraph."""
    objects = []
    pending_name = None
    for block in section["blocks"]:
        if block["t"] == "p":
            name = object_name(block["text"])
            if name and len(block["text"]) < 120:
                pending_name = name
        elif block["t"] == "table":
            rows = block["rows"]
            if not rows:
                continue
            header = [h.lower() for h in rows[0]]
            is_required = header[:2] == ["property", "required"]
            is_props = header[:1] == ["property"] and not is_required
            if is_required != required_tables or not (is_required or is_props):
                continue
            name = pending_name or ("root" if not objects else f"object{len(objects)}")
            fields = [field_row(r, is_required) for r in rows[1:] if r and r[0]]
            objects.append({"object": name, "fields": fields})
            pending_name = None
    return objects


CURL_BODY = re.compile(r"-d\s+'(.*)'", re.S)


def examples_of(section):
    request = None
    response = None
    for block in section["blocks"]:
        if block["t"] != "code":
            continue
        text = block["text"].strip()
        if text.startswith("curl"):
            m = CURL_BODY.search(text)
            if m and request is None:
                try:
                    request = json.loads(m.group(1))
                except json.JSONDecodeError:
                    pass
        elif text[:1] in "{[" and response is None:
            try:
                response = json.loads(text)
            except json.JSONDecodeError:
                pass
    return request, response


def section_text(section):
    return [b["text"] for b in section["blocks"] if b["t"] in ("p", "li")]


def extract(sections, source):
    out = {}
    for s in sections:
        sid = s["id"]
        if not sid:
            continue
        entry = {"source": source}
        props = tables_of(s, required_tables=False)
        required = tables_of(s, required_tables=True)
        request, response = examples_of(s)
        if props:
            entry["objects"] = props
        if required:
            entry["required"] = required
        if request is not None:
            entry["requestExample"] = request
        if response is not None:
            entry["responseExample"] = response
        entry["text"] = section_text(s)
        out[sid] = entry
    return out


def main():
    public_html, partner_html, out_path = sys.argv[1:4]
    # Public docs win for shared anchors; partner-only anchors are added.
    sections = extract(parse(partner_html), "partner")
    sections.update(extract(parse(public_html), "public"))
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump({"sections": sections}, f, ensure_ascii=False, indent=1, sort_keys=True)
        f.write("\n")
    print(f"wrote {len(sections)} sections to {out_path}")


if __name__ == "__main__":
    main()
