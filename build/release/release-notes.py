#!/usr/bin/env python3
"""Writes the notes of a GitHub release (Markdown, on stdout) from the metainfo file.

usage: release-notes.py METAINFO VERSION [--commit SHA] [--run-url URL] [--ostree-commit ID]

The body is the <description> of <release version="VERSION"> (paragraphs, bullet lists, <code>), followed by a footer
that says which commit and which run built the files and which OSTree commit the Flatpak repository got. Exits 1 when
the metainfo has no such release.
"""
import argparse
import sys
import xml.etree.ElementTree as ET


def text_of(element):
    """The text of an element with its inline children (<code>, <em>) as Markdown."""
    parts = [element.text or ""]
    for child in element:
        inner = text_of(child)
        parts.append("`%s`" % inner if child.tag == "code" else inner)
        parts.append(child.tail or "")
    return " ".join("".join(parts).split())


def markdown(description):
    blocks = []
    for node in description:
        if node.tag == "p":
            blocks.append(text_of(node))
        elif node.tag in ("ul", "ol"):
            items = []
            for number, item in enumerate((i for i in node if i.tag == "li"), 1):
                marker = "-" if node.tag == "ul" else "%d." % number
                items.append("%s %s" % (marker, text_of(item)))
            blocks.append("\n".join(items))
    return "\n\n".join(block for block in blocks if block)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    parser.add_argument("metainfo")
    parser.add_argument("version")
    parser.add_argument("--commit", default="")
    parser.add_argument("--run-url", default="")
    parser.add_argument("--ostree-commit", default="")
    args = parser.parse_args()

    root = ET.parse(args.metainfo).getroot()
    release = next((r for r in root.iter("release") if r.get("version") == args.version), None)
    if release is None:
        print("release-notes: no <release version=\"%s\"> in %s" % (args.version, args.metainfo), file=sys.stderr)
        return 1
    description = release.find("description")
    body = markdown(description) if description is not None else ""

    footer = []
    if args.commit:
        footer.append("Built from commit `%s`." % args.commit)
    if args.run_url:
        footer.append("Build run: %s" % args.run_url)
    if args.ostree_commit:
        footer.append("Flatpak OSTree commit of the app: `%s`." % args.ostree_commit)
    out = body or "KuickShare %s." % args.version
    if footer:
        out += "\n\n---\n" + "\n".join(footer)
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
